# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.3](https://github.com/bjackman/limmat/compare/v0.2.2...v0.2.3) - 2024-12-02

### Fixed

- Don't panic
- Note in --help that "get" is experimental

### Other

- *(dev)* Add warning about locking
- *(dev)* Notes on config repos
- *(dev)* Bug notes
- Fix new Clippy lints
- *(dev)* Notes on flock
- Remove timestamp argument from commit funcs
- Pull out TestJob::set_env
- Don't print noise when running 0 dep tests
- *(dev)* Notes
- Clarify `limmat test` intention
- I accidentally a word

## [0.2.2](https://github.com/bjackman/limmat/compare/v0.2.1...v0.2.2) - 2024-11-25

### Added

- Add favicon
- Add a proper title to the web UI tab
- Support finding stderr as well as stdout
- Implement 'get' command
- Add default for --config

### Fixed

- Fixup LIMMAT_RESOURCE_ env handling
- Move database checking into TestJob
- Move test result reporting into Job

### Other

- Use shared repo in another test
- Dump config & args to debug log
- Share repo between runs in should_find_output
- It works on MacOS
- *(dev)* Notes on bugs
- Use Default for default
- Add some debug logging
- More tests for 'test' command
- Remove cache_lookup helper
- Basic tests for 'test' command
- Some more logging tweaks
- Fiddle around with Job API
- Fix typos
- Allow running other subcommands with LimmatChildBuilder
- Create DatabaseEntry API
- Renames in database API
- Make Database::cached_result accept TestCase
- Stop dancing iterator quadrilles
- Break out helper to run dependency jobs
- Use async properly in integration tests
- *(dev)* Notes on features
- Fixup order of structs/impls
- I proofread them
- Notes on platforms & binaries
- *(dev)* Notes on packaging
- More installation notes
- *(dev)* Notes on release-plz experimentation

## [0.2.1](https://github.com/bjackman/local-ci/compare/v0.1.0...v0.1.1) - 2024-11-23

This is the first "proper" relase. I've added documentation and the initial featureset.