# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/lkalir/tocat/compare/tocat-plugins-v0.2.0...tocat-plugins-v0.2.1) - 2026-08-22

### Added

- *(plugins)* generate the encrypt cipher table from a macro
- *(plugins)* add null framing mode
- *(plugins)* add encryption plugin
- *(plugins)* limit plugins can count packets now

## [0.2.0](https://github.com/lkalir/tocat/compare/tocat-plugins-v0.1.0...tocat-plugins-v0.2.0) - 2026-08-15

### Added

- *(api)* [**breaking**] replace datagram_safe with boundary and needs enums
- *(plugins)* hex encoding/decoding plugin
- *(plugins)* frame and unframe plugins
- *(plugins)* add base64/unbase64 plugins.
- *(plugins)* added chunk logging to rate
- *(plugins)* Added native hash plugin

### Fixed

- *(plugins)* base64 accidentally marked as not datagram-safe
