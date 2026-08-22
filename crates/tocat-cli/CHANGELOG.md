# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
## [0.2.1] - 2026-08-22

### 🚀 Features

- *(plugins)* Add encryption plugin

## [0.2.0](https://github.com/lkalir/tocat/compare/tocat-v0.1.0...tocat-v0.2.0) - 2026-08-15

### Added

- *(endpoints)* unix seqpacket and datagram endpoints
- *(endpoints)* make file endpoint more aware of char and block devices
- *(endpoints)* add more aliases for tcp, udp, and unix endpoints
- *(endpoints)* tty and pty
- *(api)* [**breaking**] replace datagram_safe with boundary and needs enums
- *(plugins)* hex encoding/decoding plugin
- *(plugins)* frame and unframe plugins
- *(plugins)* add base64/unbase64 plugins.
- *(endpoints)* support fork for udp-listen
- *(cli)* propagate shutdown signals to pipeline stages as EOF
- *(plugins)* Added native hash plugin

### Fixed

- *(endpoints)* log actual local address when using tcp-listen
