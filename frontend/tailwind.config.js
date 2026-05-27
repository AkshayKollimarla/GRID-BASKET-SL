/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ["./src/**/*.{js,ts,jsx,tsx}"],
  theme: {
    extend: {
      colors: {
        // Light theme palette — clean, simple, bold.
        ink: "#0f172a",       // primary text (slate-900)
        panel: "#ffffff",      // card/panel background
        edge: "#e5e7eb",       // borders & subtle dividers
        muted: "#64748b",      // secondary text (slate-500)
        accent: "#2563eb",     // primary accent (blue-600)
        warn: "#d97706",       // amber-600
        danger: "#dc2626",     // red-600
        good: "#16a34a",       // green-600
      },
      fontFamily: {
        // Calibri-first for the whole UI, per user spec. Calibri is a
        // Windows-installed font; everywhere else we fall back to clean
        // system sans-serifs that look close.
        sans: [
          "Calibri",
          "system-ui",
          "-apple-system",
          "Segoe UI",
          "Roboto",
          "Helvetica Neue",
          "Arial",
          "sans-serif",
        ],
        // "Mono" still uses Calibri (per user request) but with
        // tabular-numerics enabled in globals.css so columns line up.
        mono: [
          "Calibri",
          "ui-monospace",
          "SFMono-Regular",
          "Menlo",
          "Consolas",
          "monospace",
        ],
        display: [
          "Calibri",
          "system-ui",
          "-apple-system",
          "Segoe UI",
          "Roboto",
          "sans-serif",
        ],
      },
    },
  },
  plugins: [],
};
