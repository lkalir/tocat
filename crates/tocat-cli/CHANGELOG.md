# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
## [0.3.0] - 2026-09-08

### 🚀 Features

- *(endpoints)* Websockets
- *(endpoints)* Proxy and socks5 layers

### 🚜 Refactor

- *(core)* [**breaking**] Most internals moved to tocat-core
- *(core)* [**breaking**] Forward plugin features through tocat-core

### 🧪 Testing

- *(core)* Made an alt frontend for testing

### ⚙️ Miscellaneous Tasks

- Updating dependencies
## [0.2.1] - 2026-08-23

### 🚀 Features

- *(plugins)* Add encryption plugin
- *(endpoints)* A queue endpoint an embedder can drive
- *(endpoints)* Common socket options for socket endpoints
- *(endpoints)* Endpoint reconnect options
- *(endpoints)* Udp multicast support
- *(endpoints)* Tls and major internal refactor for layers
- *(endpoints)* Mutual tls
- *(cli)* Dump fully resolved config when using --dump-config

### 🚜 Refactor

- *(cli)* Broke tocat into main and lib, added integration tests

### 🧪 Testing

- *(cli)* Cover unix, fork, udp, exec, and the sync path

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
