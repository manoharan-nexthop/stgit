#!/bin/sh

test_description="Test 'stg export'"

. ./test-lib.sh

test_expect_success 'Attempt export on uninitialized branch' '
    command_error stg export 2>err &&
    grep "error: no patches applied" err &&
    rm err
'

test_expect_success 'Initialize repo with patches' '
    echo "foo" >foo.txt &&
    git add foo.txt &&
    git commit -m "initial" &&
    for i in 1 2 3 4 5; do
      echo "line $i" >>foo.txt &&
      stg new -m "patch-$i" &&
      stg refresh || return 1
    done
'

test_expect_success 'Export to directory' '
    stg export -d export1 &&
    for i in 1 2 3 4 5; do
      test_path_is_file export1/patch-$i || return 1
    done
'

test_expect_success 'Export with multiple diff-opts' '
    stg export -d export2 -O --minimal -O --no-indent-heuristic &&
    for i in 1 2 3 4 5; do
      test_path_is_file export2/patch-$i || return 1
    done
'

test_expect_success 'Reimport directory export' '
    stg delete $(stg series --noprefix) &&
    stg import -S export1/series &&
    test "$(echo $(stg series --noprefix))" = \
      "patch-1 patch-2 patch-3 patch-4 patch-5" &&
    test "$(echo $(stg series -d --noprefix patch-1))" = "patch-1 # patch-1"
'

test_expect_success 'Export to stdout' '
    stg export --stdout >export2.txt &&
    head -n1 export2.txt |
    grep -e "^----------------------------"
'

test_expect_success 'Export with none applied' '
    stg pop -a &&
    command_error stg export --dir export3 2>err &&
    grep -e "no patches applied" err &&
    test_path_is_missing export3 &&
    stg push -a
'

test_expect_success 'Export with dirty working tree' '
    echo "another line" >>foo.txt &&
    stg export -d export4 patch-1 2>err &&
    grep -e "warning: Local changes in the tree" err &&
    test_path_is_file export4/series &&
    test_path_is_file export4/patch-1 &&
    git checkout foo.txt
'

test_expect_success 'Use custom template' '
    echo "%(authemail)s -- %(shortdescr)s" >template &&
    stg export -t template -p patch-1 &&
    grep -e "^author@example.com -- patch-1" patches-master/patch-1.patch
'

test_expect_success 'Export numbered patches with custom extension' '
    stg export -d export5 -n -e mydiff patch-1 patch-2 &&
    test_path_is_file export5/01-patch-1.mydiff &&
    test_path_is_file export5/02-patch-2.mydiff &&
    grep -e "02-patch-2\.mydiff" export5/series
'

test_expect_success 'Export series with empty patch' '
    stg new -m patch-6 &&
    stg export -d export6 &&
    test_path_is_file export6/patch-6 &&
    stg delete $(stg series --noprefix) &&
    stg import -S export6/series
'

test_expect_success 'Test update-series with reordered patches' '
    # Start with a clean slate  - use a new branch
    git checkout -b test-reorder &&
    stg init &&

    # Create initial set of patches
    stg new -m "001" &&
    echo "line 2" >>foo.txt &&
    stg refresh &&
    stg new -m "002" &&
    echo "line 3" >>foo.txt &&
    stg refresh &&
    stg new -m "003" &&
    echo "line 4" >>foo.txt &&
    stg refresh &&
    stg new -m "005" &&
    echo "line 5" >>foo.txt &&
    stg refresh &&
    stg new -m "006" &&
    echo "line 6" >>foo.txt &&
    stg refresh &&

    # Export all patches initially
    stg export -d export7 &&

    # Now insert a new patch between 003 and 005
    stg goto 003 &&
    stg new -m "004" &&
    echo "line 4b" >>bar.txt &&
    git add bar.txt &&
    stg refresh &&
    stg push -a &&

    # Export only patch 005 with update-series flag
    stg export -F -U -d export7 005 &&

    # The series file should keep 005 in its original position (after 003)
    # since 004 was never exported to the series file
    # The bug was that 005 was being moved to the end after 006
    cat >series-expected <<-\EOF &&
	001
	002
	003
	005
	006
	EOF

    # Remove the base commit comment line from actual series
    grep -v "^# This series applies" export7/series >series-actual &&
    test_cmp series-expected series-actual
'

test_expect_success 'Re-export existing patch with numbered series does not duplicate entry' '
    # Simulate the case where the series file was originally created with numbered
    # filenames (e.g. by a different tool) and the stg stack only covers a subset
    # of the patches in that series file.
    git checkout -b test-reexport &&
    stg init &&

    stg new -m "patch-a" &&
    echo "a" >>foo.txt &&
    stg refresh &&
    stg new -m "patch-b" &&
    echo "b" >>foo.txt &&
    stg refresh &&

    # Manually create a series file with numbered entries plus an extra entry
    # that is NOT in the current stg stack (simulating a foreign series file).
    mkdir -p export8 &&
    printf "# This series applies on Git commit abc123\n" >export8/series &&
    printf "0001-patch-a\n" >>export8/series &&
    printf "0002-patch-b\n" >>export8/series &&
    printf "0003-patch-c-not-in-stack\n" >>export8/series &&

    # Re-export patch-b with -U; it is already in the series at position 2.
    # The bug caused it to be appended at the end (after 0003-patch-c-not-in-stack).
    stg export -U -n -d export8 patch-b &&

    # With -n and only 1 patch being exported, num_width=2 so the filename
    # becomes "01-patch-b" — but crucially it must stay at position 2, not
    # be appended after the non-stack entry.
    cat >series-expected <<-\EOF &&
	# This series applies on Git commit abc123
	0001-patch-a
	01-patch-b
	0003-patch-c-not-in-stack
	EOF

    test_cmp series-expected export8/series
'

test_expect_success 'Export new patch inserts at correct position based on stack order' '
    git checkout -b test-insert &&
    stg init &&

    stg new -m "001" &&
    echo "line 2" >>foo.txt &&
    stg refresh &&
    stg new -m "002" &&
    echo "line 3" >>foo.txt &&
    stg refresh &&
    stg new -m "003" &&
    echo "line 4" >>foo.txt &&
    stg refresh &&
    stg new -m "004" &&
    echo "line 4b" >>foo.txt &&
    stg refresh &&
    stg new -m "005" &&
    echo "line 5" >>foo.txt &&
    stg refresh &&
    stg new -m "006" &&
    echo "line 6" >>foo.txt &&
    stg refresh &&

    # Export everything except 004 to create the initial series
    stg export -d export9 001 002 003 005 006 &&
    grep -v "^#" export9/series >initial-actual &&
    printf "001\n002\n003\n005\n006\n" >initial-expected &&
    test_cmp initial-expected initial-actual &&

    # Now export just 004 with -U; it should be inserted between 003 and 005
    # (the stack positions 003=2, 004=3, 005=4 drive the ordering).
    stg export -U -d export9 004 &&

    cat >series-expected <<-\EOF &&
	001
	002
	003
	004
	005
	006
	EOF

    grep -v "^#" export9/series >series-actual &&
    test_cmp series-expected series-actual
'

test_expect_success 'Re-export patch with prefix+.patch in stg name does not duplicate' '
    # When stg import -S is used WITHOUT --stripname the stg patch name keeps
    # the numeric prefix AND the .patch extension, e.g. "0237-foo.patch".
    # Re-exporting such a patch with -U must replace the series entry in-place,
    # not leave the original line AND append a second entry at the end.
    git checkout -b test-nostrip &&
    stg init &&

    stg new -m "placeholder" &&
    echo "content" >>foo.txt &&
    stg refresh &&
    stg rename "0237-my-patch.patch" &&

    mkdir -p export10 &&
    printf "0236-prev-patch.patch\n" >export10/series &&
    printf "0237-my-patch.patch\n" >>export10/series &&
    printf "0238-next-patch.patch\n" >>export10/series &&

    # Re-export the patch (patch name is "0237-my-patch.patch" in stg).
    # The exported filename with no flags is also "0237-my-patch.patch".
    # With the bug, the series gains a duplicate entry at the end.
    stg export -U -d export10 "0237-my-patch.patch" &&

    cat >series-expected <<-\EOF &&
	0236-prev-patch.patch
	0237-my-patch.patch
	0238-next-patch.patch
	EOF

    test_cmp series-expected export10/series
'

test_expect_success 'Multi-segment numeric prefix round-trip with --update-series' '
    # normalize_patch_ident must strip ALL leading digit+dash chars (like stripname()
    # in import.rs), not just the first segment.  A series entry "01-02-foo.patch"
    # should match the stg patch "foo" (imported with --stripname) and be updated
    # in-place rather than duplicated.
    git checkout -b test-multiseg &&
    stg init &&

    stg new -m "placeholder" &&
    echo "x" >>foo.txt &&
    stg refresh &&
    stg rename "foo" &&

    mkdir -p export11 &&
    printf "00-preamble.patch\n" >export11/series &&
    printf "01-02-foo.patch\n" >>export11/series &&
    printf "99-epilogue.patch\n" >>export11/series &&

    # Patch name is "foo"; existing series entry is "01-02-foo.patch".
    # After -U the entry should be replaced with "foo" (no -n, no extension)
    # at its original position — not duplicated at the end.
    stg export -U -d export11 foo &&

    cat >series-expected <<-\EOF &&
	00-preamble.patch
	foo
	99-epilogue.patch
	EOF

    test_cmp series-expected export11/series
'

test_expect_success 'format-patch does not strip subsystem or version tags from subject' '
    git checkout -b test-fp-prefix &&
    stg init &&

    stg new -m "[v5.15] net: fix something" &&
    echo "a" >>foo.txt &&
    stg refresh &&
    stg new -m "[net/ipv4] tcp: fix routing" &&
    echo "b" >>foo.txt &&
    stg refresh &&

    stg export -F -d export12 &&

    # [v5.15] and [net/ipv4] must be preserved verbatim — they are NOT [PATCH] markers.
    grep "^Subject: \[v5.15\] net: fix something" export12/v5.15-net-fix-something &&
    grep "^Subject: \[net/ipv4\] tcp: fix routing" export12/net-ipv4-tcp-fix-routing
'

test_expect_success 'format-patch Date header uses author timezone not machine timezone' '
    git checkout -b test-fp-date &&
    stg init &&

    stg new -m "timezone test" &&
    echo "tz" >>foo.txt &&
    stg refresh &&

    stg export -F -d export13 &&

    # Date: line must match RFC2822 format and include a timezone offset.
    grep -E "^Date: [A-Za-z]{3}, +[0-9]+ [A-Za-z]{3} [0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} [+-][0-9]{4}$" \
        export13/timezone-test
'

test_done
