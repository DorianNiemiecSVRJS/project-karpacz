# Project Karpacz change log

## Project Karpacz 0.2.0
**Released in February 16, 2025**

- Added a reverse proxy module (*rproxy*)
- Added `builder_without_request` method for ResponseData builder
- Added `ServerConfigurationRoot` parameter for configuration validation functions
- Fixed `BadValues` error when querying configuration by modules
- Implemented parallel function execution (by spawning a Tokio task) in ResponseData 
- Improved server configuration processing performance
- The web server now uses `async-channel` crate instead of Tokio's MPSC channel
- The web server now uses `local_dynamic_tls` feature of `mimalloc` crate to fix module loading issues

## Project Karpacz 0.1.0
**Released in February 13, 2025**

- First alpha release