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
    patch::{patchrange, PatchName, PatchRange, RangeConstraint},
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

/// Strip a leading `NNNN-` numeric prefix and any trailing file extension so that
/// series file entries can be matched against stg patch names regardless of whether
/// the stack was imported with or without `--stripname`.
///
/// Examples: `"0237-foo.patch"` → `"foo"`, `"01-bar"` → `"bar"`, `"baz.diff"` → `"baz"`.
fn normalize_patch_ident(s: &str) -> &str {
    let s = if let Some(dash_pos) = s.find('-') {
        let prefix = &s[..dash_pos];
        if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
            &s[dash_pos + 1..]
        } else {
            s
        }
    } else {
        s
    };
    if let Some(dot_pos) = s.rfind('.') {
        &s[..dot_pos]
    } else {
        s
    }
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

    // Read existing series file if it exists
    let existing_content = if series_path.exists() {
        std::fs::read_to_string(series_path)
            .with_context(|| format!("reading {series_path:?}"))?
    } else {
        String::new()
    };

    // Parse existing series file to preserve comments and non-exported patches
    let mut lines: Vec<String> = Vec::new();

    for line in existing_content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            lines.push(line.to_string());
        } else if !trimmed.is_empty() {
            lines.push(line.to_string());
        }
    }

    // Build a map of patch names to their filenames
    let mut patch_filenames: HashMap<String, String> = HashMap::new();
    for (i, patchname) in patches.iter().enumerate() {
        let patchfile_name = if numbered_flag {
            let patch_number = i + 1;
            format!("{patch_number:0num_width$}-{patchname}{extension}")
        } else {
            format!("{patchname}{extension}")
        };
        patch_filenames.insert(patchname.to_string(), patchfile_name);
    }

    // Build a set of patches being exported for quick lookup
    let exported_patches: HashSet<String> = patches.iter().map(|p| p.to_string()).collect();

    // Update or add entries for exported patches
    let mut updated_lines: Vec<String> = Vec::new();
    let mut processed_patches: HashSet<String> = HashSet::new();

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            updated_lines.push(line.clone());
        } else {
            // Extract the patch name from the line (remove numbering prefix if present)
            let line_patchname = if let Some(dash_pos) = trimmed.find('-') {
                // Check if prefix is all digits
                let prefix = &trimmed[..dash_pos];
                if prefix.chars().all(|c| c.is_ascii_digit()) {
                    &trimmed[dash_pos + 1..]
                } else {
                    trimmed
                }
            } else {
                trimmed
            };

            // Remove extension if present
            let line_patchname = if let Some(ext_pos) = line_patchname.rfind('.') {
                &line_patchname[..ext_pos]
            } else {
                line_patchname
            };

            // Check if this line corresponds to one of the exported patches.
            // Compare the normalised series-line name against both the raw patch name
            // and its normalised form, because stg patch names retain their `NNNN-`
            // prefix and `.patch` extension when the stack was imported without
            // `--stripname` (e.g. `0237-foo.patch`).
            let matching_exported = patch_filenames
                .keys()
                .find(|patchname| {
                    let p = patchname.as_str();
                    line_patchname == p || line_patchname == normalize_patch_ident(p)
                })
                .cloned();

            if let Some(patchname) = matching_exported {
                // Replace in-place with the new filename (handles format/numbering changes)
                // and mark processed so the trailing loop doesn't append a duplicate.
                let patchfile_name = patch_filenames.get(&patchname).unwrap().clone();
                updated_lines.push(patchfile_name);
                processed_patches.insert(patchname);
            } else {
                // Non-exported patch: keep it, but first insert any unprocessed exported
                // patches that belong before it according to stack order.
                // Use normalised comparison so patches imported without --stripname
                // (which retain numeric prefix/extension in their stg name) are found.
                let line_patch_pos = stack.all_patches().position(|p| {
                    let s = <PatchName as AsRef<str>>::as_ref(p);
                    s == line_patchname || normalize_patch_ident(s) == line_patchname
                });

                if let Some(line_pos) = line_patch_pos {
                    for stack_patch in stack.all_patches() {
                        let stack_patch_str = stack_patch.to_string();
                        if exported_patches.contains(&stack_patch_str)
                            && !processed_patches.contains(&stack_patch_str)
                        {
                            let stack_patch_pos = stack.all_patches()
                                .position(|p| p == stack_patch)
                                .unwrap();

                            if stack_patch_pos < line_pos {
                                if let Some(patchfile_name) = patch_filenames.get(&stack_patch_str) {
                                    updated_lines.push(patchfile_name.clone());
                                    processed_patches.insert(stack_patch_str);
                                }
                            }
                        }
                    }
                }

                updated_lines.push(line.clone());
            }
        }
    }

    // Add any remaining new patches that weren't inserted yet
    for stack_patch in stack.all_patches() {
        let stack_patch_str = stack_patch.to_string();
        if exported_patches.contains(&stack_patch_str)
            && !processed_patches.contains(&stack_patch_str) {
            if let Some(patchfile_name) = patch_filenames.get(&stack_patch_str) {
                updated_lines.push(patchfile_name.clone());
                processed_patches.insert(stack_patch_str);
            }
        }
    }

    // Write the updated series file
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

    // Match patterns like [PATCH], [RFC PATCH], [PATCH v2], [PATCH 1/3], etc.
    if let Some(stripped) = subject.strip_prefix('[') {
        if let Some(pos) = stripped.find(']') {
            let prefix = &stripped[..pos];
            // Check if it looks like a patch prefix
            if prefix.split_whitespace().any(|word| {
                word.eq_ignore_ascii_case("PATCH")
                    || word.eq_ignore_ascii_case("RFC")
                    || word.starts_with("v")
                    || word.contains('/')
            }) {
                return stripped[pos + 1..].trim_start().to_string();
            }
        }
    }

    subject.to_string()
}

/// Format a git time in RFC2822 format (e.g., "Mon, 16 Nov 2020 18:11:47 -0800")
fn format_rfc2822(time: &gix::date::Time) -> String {
    use std::time::{Duration, UNIX_EPOCH};

    let timestamp = UNIX_EPOCH + Duration::from_secs(time.seconds as u64);
    let offset_seconds = time.offset;

    // Convert to jiff::Zoned for formatting
    if let Ok(zoned) = jiff::Zoned::try_from(timestamp) {
        // Adjust for the timezone offset
        let offset_hours = offset_seconds / 3600;
        let offset_mins = (offset_seconds.abs() % 3600) / 60;
        let offset_str = format!("{:+03}{:02}", offset_hours, offset_mins);

        // Format: "Mon, 16 Nov 2020 18:11:47 -0800"
        let formatted = zoned.strftime("%a, %d %b %Y %H:%M:%S");
        format!("{} {}", formatted, offset_str)
    } else {
        // Fallback to DEFAULT format if conversion fails
        time.format(gix::date::time::format::DEFAULT).to_string()
    }
}
