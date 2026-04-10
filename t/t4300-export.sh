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

test_done
