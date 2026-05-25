// SPDX-License-Identifier: GPL-2.0-only

//! `stg export` implementation.

use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use bstr::BStr;
use clap::{Arg, ArgGroup};

use crate::{
    argset,
    branchloc::BranchLocator,
    ext::{CommitExtended, RepositoryExtended},
    patch::{patchrange, PatchRange, RangeConstraint},
    stack::{InitializationPolicy, Stack, StackAccess, StackStateAccess},
    stupid::Stupid,
};

pub(super) const STGIT_COMMAND: super::StGitCommand = super::StGitCommand {
    name: "export",
    category: super::CommandCategory::StackInspection,
    make,
    run,
};

fn make() -> clap::Command {
    clap::Command::new(STGIT_COMMAND.name)
        .about("Export patches to a directory")
        .long_about(
            "Export a range of patches to a given directory in unified diff format. \
             All applied patches are exported by default.\n\
             \n\
             Patches are exported to 'patches-<branch>' by default. The '--dir' option \
             may be used to specify a different output directory.\n\
             \n\
             The patch file output may be customized via a template file found at \
             \"$GIT_DIR/patchexport.tmpl\", \"~/.stgit/templates/patchexport.tmpl\", \
             or \"$(prefix)/share/stgit/templates\". The following variables are \
             supported in the template file:\n\
             \n    %(description)s - patch description\
             \n    %(shortdescr)s  - the first line of the patch description\
             \n    %(longdescr)s   - the rest of the patch description, after the first line\
             \n    %(diffstat)s    - the diff statistics\
             \n    %(authname)s    - author name\
             \n    %(authemail)s   - author email\
             \n    %(authdate)s    - patch creation date (ISO-8601 format)\
             \n    %(commname)s    - committer name\
             \n    %(commemail)s   - committer email",
        )
        .arg(
            Arg::new("patchranges")
                .help("Patches to export")
                .long_help(
                    "Patches to export.\n\
                     \n\
                     A patch name or patch range of the form \
                     '[begin-patch]..[end-patch]' may be specified.",
                )
                .value_name("patch")
                .num_args(1..)
                .allow_hyphen_values(true)
                .value_parser(clap::value_parser!(PatchRange)),
        )
        .arg(argset::branch_arg())
        .arg(
            Arg::new("dir")
                .long("dir")
                .short('d')
                .help("Export patches to <dir> instead of the default")
                .value_name("dir")
                .value_hint(clap::ValueHint::DirPath)
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("patch")
                .long("patch")
                .short('p')
                .help("Suffix patch file names with \".patch\"")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("extension")
                .long("extension")
                .short('e')
                .help("Suffix patch file names with \".<ext>\"")
                .conflicts_with("patch")
                .num_args(1)
                .value_name("ext"),
        )
        .arg(
            Arg::new("numbered")
                .long("numbered")
                .short('n')
                .help("Prefix patch file names with order numbers.")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("template")
                .long("template")
                .short('t')
                .help("Use <file> as template")
                .value_name("file")
                .value_hint(clap::ValueHint::FilePath)
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("stdout")
                .long("stdout")
                .short('s')
                .help("Export to stdout instead of directory")
                .conflicts_with("dir")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("no-series")
                .long("no-series")
                .short('N')
                .help("Do not generate or update the series file")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("stdout"),
        )
        .arg(
            Arg::new("update-series")
                .long("update-series")
                .short('U')
                .help("Update existing series file instead of replacing it")
                .long_help(
                    "Update the existing series file by adding or updating entries for \
                     the exported patches, rather than replacing the entire file. \
                     Patches not being exported will remain in the series file."
                )
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("stdout"),
        )
        .group(
            ArgGroup::new("series-mode")
                .args(["no-series", "update-series"])
                .required(false),
        )
        .arg(
            Arg::new("format-patch")
                .long("format-patch")
                .short('F')
                .help("Use git format-patch style format")
                .long_help(
                    "Use git format-patch style format. This strips [PATCH] and similar \
                     prefixes from the subject line and uses a format similar to \
                     'git format-patch' output."
                )
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("template"),
        )
        .arg(argset::diff_opts_arg())
}

/// Normalise a patch filename or stg patch name to its base identifier so that
/// series file entries can be matched against stg patch names regardless of whether
/// the stack was imported with or without `--stripname`.
///
/// Mirrors `stripname()` in `import.rs`: greedily strips all leading digit+dash
/// characters, then removes a trailing `.patch` or `.diff` extension only.
///
/// Examples: `"0237-foo.patch"` → `"foo"`, `"01-02-bar"` → `"bar"`, `"baz.diff"` → `"baz"`.
fn normalize_patch_ident(s: &str) -> &str {
    let stripped = s.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
    // Only apply the prefix strip when there is a non-empty text component left.
    // Pure-numeric names like "001" must remain as-is (stripping would give "").
    let s = if stripped.is_empty() { s } else { stripped };
    s.strip_suffix(".patch")
        .or_else(|| s.strip_suffix(".diff"))
        .unwrap_or(s)
}

/// Update an existing series file by adding or updating entries for exported patches.
fn update_series_file(
    series_path: &Path,
    patches: &[crate::patch::PatchName],
    stack: &Stack,
    numbered_flag: bool,
    num_width: usize,
    extension: &str,
) -> Result<()> {
    use std::collections::HashSet;

    let existing_content = if series_path.exists() {
        std::fs::read_to_string(series_path)
            .with_context(|| format!("reading {series_path:?}"))?
    } else {
        String::new()
    };

    // Preserve every line — including blank separators — as-is.
    let lines: Vec<String> = existing_content.lines().map(str::to_string).collect();

    // Build patch-name → output-filename map.
    let mut patch_filenames: HashMap<String, String> = HashMap::new();
    for (i, patchname) in patches.iter().enumerate() {
        let patchfile_name = if numbered_flag {
            format!("{:0num_width$}-{patchname}{extension}", i + 1)
        } else {
            format!("{patchname}{extension}")
        };
        patch_filenames.insert(patchname.to_string(), patchfile_name);
    }

    // Precompute once to avoid O(n³) repeated iteration over the stack.
    //
    // normalized-name → stack position
    let stack_positions: HashMap<String, usize> = stack
        .all_patches()
        .enumerate()
        .map(|(i, p)| (normalize_patch_ident(p.as_ref()).to_string(), i))
        .collect();

    // normalized-name → original patch-name (for series-line → exported-patch matching)
    let normalized_exported: HashMap<String, String> = patch_filenames
        .keys()
        .map(|name| (normalize_patch_ident(name.as_str()).to_string(), name.clone()))
        .collect();

    // Exported patches sorted by stack position for ordered insertion.
    let mut exported_by_pos: Vec<(usize, String)> = patch_filenames
        .keys()
        .filter_map(|name| {
            stack_positions
                .get(normalize_patch_ident(name.as_str()))
                .map(|&pos| (pos, name.clone()))
        })
        .collect();
    exported_by_pos.sort_unstable_by_key(|(pos, _)| *pos);

    let mut updated_lines: Vec<String> = Vec::new();
    let mut processed_patches: HashSet<String> = HashSet::new();

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            updated_lines.push(line.clone());
            continue;
        }

        // Normalize the series-line identifier using the same algorithm as
        // stripname() in import.rs: greedy digit/dash strip + .patch/.diff only.
        let line_norm = normalize_patch_ident(trimmed);

        if let Some(patchname) = normalized_exported.get(line_norm).cloned() {
            // This series entry corresponds to one of the exported patches.
            // Replace in-place (handles numbering/extension changes) and mark
            // processed so the trailing loop doesn't append a duplicate.
            updated_lines.push(patch_filenames[&patchname].clone());
            processed_patches.insert(patchname);
        } else {
            // Non-exported line: keep it, but first insert any unprocessed
            // exported patches that belong before it in stack order.
            if let Some(&line_pos) = stack_positions.get(line_norm) {
                for (patch_pos, patchname) in &exported_by_pos {
                    if *patch_pos >= line_pos {
                        break; // vec is sorted — nothing further can be earlier
                    }
                    if !processed_patches.contains(patchname) {
                        updated_lines.push(patch_filenames[patchname].clone());
                        processed_patches.insert(patchname.clone());
                    }
                }
            }
            updated_lines.push(line.clone());
        }
    }

    // Append any exported patches not yet placed (new patches with no existing
    // series entry, or patches whose surrounding context wasn't in the stack).
    for (_, patchname) in &exported_by_pos {
        if !processed_patches.contains(patchname) {
            updated_lines.push(patch_filenames[patchname].clone());
            processed_patches.insert(patchname.clone());
        }
    }

    let mut content = updated_lines.join("\n");
    if !content.ends_with('\n') {
        content.push('\n');
    }
    std::fs::write(series_path, content)
        .with_context(|| format!("writing {series_path:?}"))?;

    Ok(())
}

fn run(matches: &clap::ArgMatches) -> Result<()> {
    let repo = gix::Repository::open()?;
    let opt_branch = matches.get_one::<BranchLocator>("branch");
    let stack =
        Stack::from_branch_locator(&repo, opt_branch, InitializationPolicy::AllowUninitialized)?;
    let stupid = repo.stupid();

    if opt_branch.is_none()
        && repo
            .stupid()
            .statuses(None)?
            .check_worktree_clean()
            .is_err()
    {
        crate::print_warning_message(
            matches,
            "Local changes in the tree; you might want to commit them first",
        );
    }

    let patches = if let Some(range_specs) = matches.get_many::<PatchRange>("patchranges") {
        patchrange::resolve_names(
            &stack,
            range_specs,
            RangeConstraint::VisibleWithAppliedBoundary,
        )?
    } else {
        stack.applied().to_vec()
    };

    if patches.is_empty() {
        return Err(super::Error::NoAppliedPatches.into());
    }

    let default_output_dir;
    let output_dir = if let Some(dir) = matches.get_one::<PathBuf>("dir").map(PathBuf::as_path) {
        dir
    } else {
        default_output_dir = format!("patches-{}", stack.get_branch_name());
        Path::new(default_output_dir.as_str())
    };

    let custom_extension;
    let extension = if let Some(custom_ext) = matches.get_one::<String>("extension") {
        custom_extension = format!(".{custom_ext}");
        custom_extension.as_str()
    } else if matches.get_flag("patch") {
        ".patch"
    } else {
        ""
    };

    let numbered_flag = matches.get_flag("numbered");
    let num_width = std::cmp::max(patches.len().to_string().len(), 2);

    let diff_opts = argset::get_diff_opts(matches, &repo.config_snapshot(), false, true);

    let format_patch_flag = matches.get_flag("format-patch");
    let template = if let Some(template_file) = matches.get_one::<PathBuf>("template") {
        Cow::Owned(std::fs::read_to_string(template_file)?)
    } else if format_patch_flag {
        Cow::Borrowed(crate::templates::PATCHEXPORT_FORMAT_PATCH_TMPL)
    } else {
        match crate::templates::get_template(&repo, "patchexport.tmpl") {
            Ok(Some(template)) => Cow::Owned(template),
            Ok(None) => Cow::Borrowed(crate::templates::PATCHEXPORT_TMPL),
            Err(e) => return Err(e),
        }
    };

    let need_diffstat = template.contains("%(diffstat)");
    let need_clean_subject = template.contains("%(shortdescr-clean)");

    let stdout_flag = matches.get_flag("stdout");
    let mut series = format!(
        "# This series applies on Git commit {}\n",
        stack.base().id()
    );

    if !stdout_flag {
        std::fs::create_dir_all(output_dir).with_context(|| format!("creating {output_dir:?}"))?;
    }

    for (i, patchname) in patches.iter().enumerate() {
        let patchfile_name = if numbered_flag {
            let patch_number = i + 1;
            format!("{patch_number:0num_width$}-{patchname}{extension}")
        } else {
            format!("{patchname}{extension}")
        };

        series.push_str(&patchfile_name);
        series.push('\n');

        let patch_commit = stack.get_patch_commit(patchname);
        let parent_commit = patch_commit.get_parent_commit()?;

        let mut replacements: HashMap<&str, Cow<'_, BStr>> = HashMap::new();
        let message = patch_commit.message_ex();
        let description = message.decode()?;
        let description = description.as_ref();
        let (shortdescr, longdescr) = if let Some((shortdescr, rest)) = description.split_once('\n')
        {
            let longdescr = rest.trim_start_matches('\n').trim_end();
            (shortdescr, longdescr)
        } else {
            (description, "")
        };
        replacements.insert("description", Cow::Borrowed(description.into()));
        replacements.insert("shortdescr", Cow::Borrowed(shortdescr.into()));
        replacements.insert("longdescr", Cow::Borrowed(longdescr.into()));

        if need_clean_subject {
            let shortdescr_clean = strip_patch_prefix(shortdescr);
            replacements.insert("shortdescr-clean", Cow::Owned(shortdescr_clean.into()));
        }
        let author = patch_commit.author()?;
        replacements.insert("authname", Cow::Borrowed(author.name));
        replacements.insert("authemail", Cow::Borrowed(author.email));

        // Use RFC2822 format for format-patch style, ISO8601 for default
        let authdate_str = if format_patch_flag {
            format_rfc2822(&author.time()?)
        } else {
            author.time()?.format(gix::date::time::format::ISO8601).to_string()
        };

        replacements.insert(
            "authdate",
            Cow::Owned(authdate_str.into()),
        );
        let committer = patch_commit.committer()?;
        replacements.insert("commname", Cow::Borrowed(committer.name));
        replacements.insert("commemail", Cow::Borrowed(committer.email));
        replacements.insert(
            "commdate",
            Cow::Owned(
                committer
                    .time()?
                    .format(gix::date::time::format::ISO8601)
                    .into(),
            ),
        );

        let diff = stupid.diff_tree_patch(
            parent_commit.tree_id()?.detach(),
            patch_commit.tree_id()?.detach(),
            <Option<Vec<OsString>>>::None,
            false,
            diff_opts.iter(),
        )?;

        if need_diffstat {
            replacements.insert(
                "diffstat",
                if parent_commit.tree_id()? == patch_commit.tree_id()? {
                    Cow::Borrowed("".into())
                } else {
                    Cow::Owned(stupid.diffstat(diff.as_ref())?)
                },
            );
        }

        let specialized = crate::templates::specialize_template(&template, &replacements);

        if stdout_flag {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            if patches.len() > 1 {
                write!(
                    stdout,
                    "{0:->79}\n\
                     {patchfile_name}\n\
                     {0:->79}\n",
                    '-'
                )?;
            }
            stdout.write_all(&specialized)?;
            stdout.write_all(&diff)?;
        } else {
            let mut file = std::fs::File::options()
                .write(true)
                .create(true)
                .truncate(true)
                .open(output_dir.join(&patchfile_name))
                .with_context(|| format!("opening {patchfile_name}"))?;
            file.write_all(&specialized)?;
            file.write_all(&diff)?;
        }
    }

    if !stdout_flag {
        let no_series = matches.get_flag("no-series");
        let update_series = matches.get_flag("update-series");

        if !no_series {
            let series_path = output_dir.join("series");

            if update_series {
                // Update mode: merge with existing series file
                update_series_file(&series_path, &patches, &stack, numbered_flag, num_width, extension)?;
            } else {
                // Default mode: replace entire series file
                std::fs::write(&series_path, series.as_str())
                    .with_context(|| format!("writing {series_path:?}"))?;
            }
        }
    }

    Ok(())
}

/// Strip [PATCH], [RFC PATCH], and similar prefixes from subject line.
fn strip_patch_prefix(subject: &str) -> String {
    let subject = subject.trim();

    // Only strip tags that contain "PATCH" or "RFC", e.g. [PATCH], [RFC PATCH], [PATCH v2 1/3].
    // Do NOT strip kernel version/subsystem tags like [v5.15] or [net/ipv4].
    if let Some(stripped) = subject.strip_prefix('[') {
        if let Some(pos) = stripped.find(']') {
            let prefix = &stripped[..pos];
            if prefix.split_whitespace().any(|word| {
                word.eq_ignore_ascii_case("PATCH") || word.eq_ignore_ascii_case("RFC")
            }) {
                return stripped[pos + 1..].trim_start().to_string();
            }
        }
    }

    subject.to_string()
}

/// Format a git time in RFC2822 format (e.g., "Mon, 16 Nov 2020 18:11:47 -0800").
///
/// Uses gix-date's GIT_RFC2822 formatter which reads `time.offset` directly,
/// avoiding both the local-timezone bug and the i64→u64 wrapping issue.
fn format_rfc2822(time: &gix::date::Time) -> String {
    time.format(gix::date::time::format::GIT_RFC2822).to_string()
}
