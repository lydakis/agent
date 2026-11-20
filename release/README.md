# Releasing

Main's manifests always say `0.0.0`, as Codex's do. A release is a tag on
main; nothing is bumped in a pull request. The release workflow writes the
tag's version into the manifests and `Cargo.lock` of its own build tree
([stamp_version.py](stamp_version.py)) before it tests or builds, and the
source archive Homebrew builds from carries those stamped manifests.

A build from main says `0.0.0 (COMMIT)` to `--version`, so it never passes
for a release, and `sonny upgrade` sees every release as newer than it.

## Checklist

1. Check that nothing retires at this release: one-off upgrade work names
   the release it is deleted in, and the release build's tests fail on any
   left. In a checkout of main:

   ```sh
   python3 release/stamp_version.py X.Y.Z
   cargo test --locked -p sonny nothing_outlives_its_release
   git checkout -- Cargo.toml Cargo.lock client/Cargo.toml sonny/Cargo.toml
   ```

2. Tag main's commit and push the tag: `git tag vX.Y.Z COMMIT && git push
   origin vX.Y.Z`. The Release workflow makes a draft release with the
   source archive, both formulas and the static Linux builds (about 25
   minutes).
3. Write the release notes on the draft, then publish it. Publishing runs
   Publish Homebrew, which updates the tap (about 20 minutes).
4. After tagging, regenerate the release store fixture from the tag
   (`python3 release/make_store_fixture.py vX.Y.Z`, which writes
   `tests/fixtures/store-vX.Y.Z.sql`) and add it in the next PR, keeping the
   last two releases' fixtures.

A tag the workflow refused, or a draft that failed, is deleted with its tag
and made again from main; main itself never changes for a release.
