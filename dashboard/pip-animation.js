// Arc-reactor animation for the PiP canvas. The canvas feeds the PiP
// <video> via captureStream(); every frame we draw gets reflected into
// the floating window. Keeps the same cyan HUD aesthetic as the main
// arc reactor on the dashboard.
//
// Exposed entry points:
//   window.__startPipAnimation(canvasId)  — begin redrawing the named canvas
//   window.__stopPipAnimation()           — cancel the loop
//
// Called by `dashboard/src/jarvis/pip.rs` from pin_pip / unpin_pip.
(function () {
    "use strict";

    let rafId = null;
    let startedAt = 0;
    let lastDrawAt = 0;
    // Target 24 fps to match the captureStream rate. rAF would otherwise
    // fire at ~60 Hz and burn CPU drawing frames the capture won't sample.
    const FRAME_MS = 1000 / 24;

    function tick(canvas, ctx) {
        const now = performance.now();
        if (now - lastDrawAt < FRAME_MS) {
            rafId = window.requestAnimationFrame(() => tick(canvas, ctx));
            return;
        }
        lastDrawAt = now;
        const t = (now - startedAt) / 1000; // seconds since start

        const W = canvas.width;
        const H = canvas.height;
        const cx = W / 2;
        const cy = H / 2;

        // Background: deep navy with a faint radial vignette.
        ctx.clearRect(0, 0, W, H);
        const bg = ctx.createRadialGradient(cx, cy, 0, cx, cy, W * 0.7);
        bg.addColorStop(0, "#0a1628");
        bg.addColorStop(1, "#020617");
        ctx.fillStyle = bg;
        ctx.fillRect(0, 0, W, H);

        // Outer tick ring (12 segments, slowly rotating).
        const outerR = W * 0.42;
        const tickRot = t * 0.35;
        ctx.strokeStyle = "rgba(34, 211, 238, 0.55)";
        ctx.lineWidth = 1.2;
        for (let i = 0; i < 12; i++) {
            const a = (Math.PI * 2 * i) / 12 + tickRot;
            const r1 = outerR;
            const r2 = outerR + 8;
            ctx.beginPath();
            ctx.moveTo(cx + Math.cos(a) * r1, cy + Math.sin(a) * r1);
            ctx.lineTo(cx + Math.cos(a) * r2, cy + Math.sin(a) * r2);
            ctx.stroke();
        }

        // Outer ring.
        ctx.strokeStyle = "rgba(34, 211, 238, 0.75)";
        ctx.lineWidth = 1.6;
        ctx.beginPath();
        ctx.arc(cx, cy, outerR, 0, Math.PI * 2);
        ctx.stroke();

        // Rotating sweep arc (the "radar" line that gives it motion).
        const sweep = t * 1.4;
        const grad = ctx.createLinearGradient(
            cx + Math.cos(sweep) * outerR,
            cy + Math.sin(sweep) * outerR,
            cx,
            cy
        );
        grad.addColorStop(0, "rgba(34, 211, 238, 0.0)");
        grad.addColorStop(1, "rgba(34, 211, 238, 0.75)");
        ctx.strokeStyle = grad;
        ctx.lineWidth = 2.4;
        ctx.beginPath();
        ctx.arc(cx, cy, outerR - 4, sweep - 0.9, sweep);
        ctx.stroke();

        // Middle ring + pulse. Pulse drives radius modulation off a 1.5s sine.
        const pulse = 0.5 + 0.5 * Math.sin(t * Math.PI * 1.33);
        const midR = W * 0.27 + pulse * 4;
        ctx.strokeStyle = "rgba(34, 211, 238, 0.6)";
        ctx.lineWidth = 1.4;
        ctx.beginPath();
        ctx.arc(cx, cy, midR, 0, Math.PI * 2);
        ctx.stroke();

        // Inner ring.
        ctx.strokeStyle = "rgba(125, 211, 252, 0.9)";
        ctx.lineWidth = 1.8;
        ctx.beginPath();
        ctx.arc(cx, cy, W * 0.12, 0, Math.PI * 2);
        ctx.stroke();

        // Core glow.
        const core = ctx.createRadialGradient(cx, cy, 0, cx, cy, W * 0.1);
        core.addColorStop(0, "rgba(186, 230, 253, 1)");
        core.addColorStop(0.4, "rgba(34, 211, 238, 0.85)");
        core.addColorStop(1, "rgba(34, 211, 238, 0)");
        ctx.fillStyle = core;
        ctx.beginPath();
        ctx.arc(cx, cy, W * 0.1, 0, Math.PI * 2);
        ctx.fill();

        // Center dot.
        ctx.fillStyle = "#ecfeff";
        ctx.beginPath();
        ctx.arc(cx, cy, 3, 0, Math.PI * 2);
        ctx.fill();

        rafId = window.requestAnimationFrame(() => tick(canvas, ctx));
    }

    window.__startPipAnimation = function (canvasId) {
        const canvas = document.getElementById(canvasId);
        if (!canvas) return;
        // Size the backing canvas so the PiP video has real pixels to show.
        // 240×240 is small enough to be cheap and large enough to look
        // sharp in Chrome's PiP overlay (~300px wide by default).
        if (canvas.width !== 240) {
            canvas.width = 240;
            canvas.height = 240;
        }
        const ctx = canvas.getContext("2d");
        if (!ctx) return;
        if (rafId !== null) {
            window.cancelAnimationFrame(rafId);
            rafId = null;
        }
        startedAt = performance.now();
        tick(canvas, ctx);
    };

    window.__stopPipAnimation = function () {
        if (rafId !== null) {
            window.cancelAnimationFrame(rafId);
            rafId = null;
        }
    };
})();
