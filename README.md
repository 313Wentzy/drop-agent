# Drop Mazter Agent

The Drop Mazter Agent is a lightweight desktop companion app that connects to [dropmazter.com](https://dropmazter.com) to deliver real-time drop timing overlays directly onto your Fortnite game. It runs silently in your system tray and activates only when Fortnite is running.

---

## How does it work?

1. You log in with your Drop Mazter account.
2. The agent detects when Fortnite is running.
3. It captures a small region of your screen to read the bus path.
4. Your selected drop map's timing data is overlaid directly onto your game via a transparent overlay.
5. When Fortnite closes, the agent goes back to sleep.

The agent communicates with Drop Mazter's servers over a secure WebSocket connection to sync your calculator settings, subscription status, and drop map data.

---

## Will it cause any delay or lag?

**No.** The agent is built in Rust, a systems programming language known for near-zero overhead. Here's why it won't affect your gameplay:

- **Tiny memory footprint** — the agent typically uses under 30 MB of RAM while active.
- **No injection** — it does not inject into Fortnite's process, modify game files, or hook into the game engine. It runs as a completely separate process.
- **Overlay is hardware-accelerated** — the transparent overlay uses standard Windows APIs and does not interfere with your game's rendering pipeline.
- **Screen capture is targeted** — it captures only a small region of pixels for bus path detection, not your full screen. This takes less than a millisecond.
- **Idle when not needed** — when Fortnite is not running, the agent sleeps with virtually zero CPU usage.

---

## What access does it have on my PC?

The agent only accesses what it needs to function:

| Access | Why |
|---|---|
| **Screen capture** (small region) | To read the bus path from the Fortnite lobby screen |
| **Overlay window** | To display drop timing data on top of your game |
| **Network (HTTPS/WSS)** | To communicate with dropmazter.com for auth, settings, and map data |
| **System tray** | To provide a tray icon with right-click menu |
| **Startup registry** (optional) | Only if you enable "Run on startup" |
| **Local config file** | To store your auth token locally so you stay logged in |

The agent does **not**:
- Read or modify any game files
- Access your filesystem beyond its own config
- Inject code into any process
- Collect or transmit personal data beyond what's described in our [Privacy Policy](https://dropmazter.com/privacy-policy)
- Install background services or drivers

---

## Why should I trust it?

- **Open source** — this repository contains the full source code. You can read every line, build it yourself, and verify exactly what it does.
- **No game modification** — the agent never touches Fortnite's files or memory. It's an independent overlay, similar to Discord's overlay or NVIDIA's FPS counter.
- **Transparent data handling** — the only data sent to our servers is your auth token and calculator settings. See our [Privacy Policy](https://dropmazter.com/privacy-policy).
- **Built in Rust** — Rust's memory safety guarantees eliminate entire classes of vulnerabilities (buffer overflows, use-after-free, etc.).
- **Auto-updates from GitHub** — updates are pulled directly from this public repository, so you can always verify what changed between versions.

---

## Is it a paid feature?

**Yes.** The Drop Mazter Agent requires:

1. A **Drop Mazter Calculator** — available at [dropmazter.com](https://dropmazter.com)
2. An **active subscription** — to access premium drop maps and real-time overlays

Free trials are available through our Discord — join at [discord.gg/dropmazter](https://discord.gg/dropmazter) to get started.

---

## Auto-Updates

The agent checks for updates:
- **On every launch**
- **Every 24 hours** while running

When an update is available, it downloads the latest version from this GitHub repository, replaces itself, and restarts automatically. No manual intervention needed.

---

## Links

- [Drop Mazter Website](https://dropmazter.com)
- [Discord](https://discord.com/invite/dropmazter)
- [X (Twitter)](https://x.com/dropmazter)
- [TikTok](https://www.tiktok.com/@dropmazter)
- [Privacy Policy](https://dropmazter.com/privacy-policy)
- [Terms of Service](https://dropmazter.com/terms-of-service)
- [EULA](https://dropmazter.com/eula)

---

## Legal

© 2026 Zix7, trading as Drop Mazter. All rights reserved.

Drop Mazter is not affiliated with, endorsed, or sponsored by Epic Games, Inc. Fortnite and related materials are trademarks and copyrights of Epic Games, Inc. All other trademarks are the property of their respective owners.
