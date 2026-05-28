/** @type {import('tailwindcss').Config} */
module.exports = {
  content: [
    "./index.html",
    "./src/**/*.rs",
  ],
  theme: {
    extend: {
      colors: {
        // Legacy zinc-blue used by the wizard and current pillars
        jarvis: {
          50:  '#eff6ff',
          400: '#60a5fa',
          500: '#3b82f6',
          600: '#2563eb',
          900: '#1e3a8a',
        },
        // ARC reactor cyan — Stark-suit HUD palette
        arc: {
          50:  '#ecfeff',
          100: '#cffafe',
          200: '#a5f3fc',
          300: '#67e8f9',
          400: '#22d3ee',
          500: '#06b6d4',
          600: '#0891b2',
          700: '#0e7490',
          800: '#155e75',
          900: '#164e63',
        },
        // Deep navy / black for the HUD background and chrome
        hud: {
          bg:     '#020617',
          panel:  '#0a0f1c',
          border: '#1e293b',
          accent: '#22d3ee',
          warn:   '#f59e0b',
          err:    '#f43f5e',
        },
        eustress: {
          400: '#fbbf24',
          500: '#f59e0b',
          600: '#d97706',
          900: '#78350f',
        },
      },
      fontFamily: {
        sans: ['system-ui', '-apple-system', 'Segoe UI', 'Roboto', 'sans-serif'],
        mono: ['JetBrains Mono', 'Menlo', 'Consolas', 'monospace'],
      },
      animation: {
        'pulse-slow':      'pulse 3s ease-in-out infinite',
        'glow':            'glow 2s ease-in-out infinite',
        'arc-spin':        'arc-spin 18s linear infinite',
        'arc-spin-fast':   'arc-spin 8s linear infinite',
        'arc-spin-rev':    'arc-spin 22s linear infinite reverse',
        'arc-pulse':       'arc-pulse 2.6s ease-in-out infinite',
        'arc-pulse-fast':  'arc-pulse 0.9s ease-in-out infinite',
        'scan':            'scan 6s linear infinite',
      },
      keyframes: {
        glow: {
          '0%, 100%': { boxShadow: '0 0 24px rgba(245, 158, 11, 0.25)' },
          '50%':      { boxShadow: '0 0 48px rgba(245, 158, 11, 0.75)' },
        },
        'arc-spin': {
          from: { transform: 'rotate(0deg)' },
          to:   { transform: 'rotate(360deg)' },
        },
        'arc-pulse': {
          '0%, 100%': { opacity: '0.55', filter: 'drop-shadow(0 0 6px rgba(34, 211, 238, 0.45))' },
          '50%':      { opacity: '1',    filter: 'drop-shadow(0 0 18px rgba(34, 211, 238, 0.95))' },
        },
        scan: {
          '0%':   { transform: 'translateY(-100%)', opacity: '0' },
          '10%':  { opacity: '0.8' },
          '90%':  { opacity: '0.8' },
          '100%': { transform: 'translateY(100vh)', opacity: '0' },
        },
      },
    },
  },
  plugins: [],
};
