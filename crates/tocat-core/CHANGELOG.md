# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
## [0.3.0] - 2026-09-08

### 🚀 Features

- *(core)* Add noise layer
- *(endpoints)* Tun/tap endpoint

### 🐛 Bug Fixes

- *(endpoints)* Read PEM through rustls-pki-types
- *(endpoints)* Fix clippy lints

### 🚜 Refactor

- *(core)* [**breaking**] Most internals moved to tocat-core
- *(core)* [**breaking**] Forward plugin features through tocat-core

### 🧪 Testing

- *(endpoints)* Noise and tuntap tests

### ⚙️ Miscellaneous Tasks

- Updating dependencies
