# Security Policy 🔒 (HYDRA-UMC-SWARM-SYNC)

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.x.x  | ✅ Yes             |

## Reporting a Vulnerability

**CRITICAL: Do not report safety-critical vulnerabilities through public GitHub issues.**

In a multi-robot swarm, a synchronization flaw can cause catastrophic physical collisions. If you discover a vulnerability affecting the **PTP Grandmaster election**, **timestamp spoofing**, or **clock drift manipulation**:

1. **Email**: Send a detailed report to `electrohobby3d@gmail.com`.
2. **Impact**: Describe if the bug allows desynchronizing robots to cause collisions, bypassing atomic start commands, or crashing the swarm heartbeat.
3. **Response**: Initial acknowledgment within 48 hours.

We follow a coordinated disclosure policy to ensure hardware safety before public release.
