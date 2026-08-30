#!/bin/sh
# Assert a test suite actually ran, and passed at least a floor of tests.
#
# The risk being guarded is a suite that silently runs nothing: a skipped
# security test reporting success is worse than a missing one. An *exact*
# count guards that too, but makes every added test a CI edit, and greets the
# contributor who adds one with a red build claiming the suite did not run.
# A floor catches the real failure and tolerates growth.
assert_ran() {
  output=$1
  floor=$2
  message=$3
  passed=$(printf '%s\n' "$output" | sed -n 's/^test result: ok\. \([0-9][0-9]*\) passed.*/\1/p' | head -1)
  if [ -z "$passed" ]; then
    echo "::error::$message (no passing test result line)"
    return 1
  fi
  if [ "$passed" -lt "$floor" ]; then
    echo "::error::$message (only $passed passed, expected at least $floor)"
    return 1
  fi
  echo "ok: $passed passed (floor $floor)"
}
