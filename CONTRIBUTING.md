# Contributing to HYDRA-UMC-SWARM-SYNC 🦾

We welcome contributions to the nanosecond-precision heartbeat of the HYDRA-UMC platform.

## Technology Stack
- **Language**: C11.
- **Protocol**: PTP (Precision Time Protocol) / IEEE 1588-2019.
- **Hardware**: STM32H745 (Hardware Timers), Raspberry Pi CM5 (BCM2712 PTP).
- **Environment**: Linux (Real-time Kernel) and Bare-metal firmware.

## Guidelines
1. **Timing Precision**: All changes to the clock synchronization algorithm must be validated with an oscilloscope or high-speed logic analyzer.
2. **Jitter Management**: Avoid any non-deterministic code in the PTP packet processing interrupt handlers.
3. **Hardware Timestamping**: Ensure all network drivers support hardware-level timestamping (SO_TIMESTAMPING).
4. **Resilience**: Test the synchronization stability under high network load and during simulated packet drops.
