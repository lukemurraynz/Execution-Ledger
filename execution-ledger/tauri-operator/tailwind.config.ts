export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        brand: {
          DEFAULT: "var(--el-color-brand)",
          hover: "var(--el-color-brand-hover)",
          light: "var(--el-color-brand-light)",
          subtle: "var(--el-color-brand-subtle)",
        },
        canvas: "var(--el-color-bg-canvas)",
        surface: "var(--el-color-bg-surface)",
        elevated: "var(--el-color-bg-elevated)",
        raised: "var(--el-color-bg-raised)",
        overlay: "var(--el-color-bg-overlay)",
        text: {
          primary: "var(--el-color-text-primary)",
          secondary: "var(--el-color-text-secondary)",
          heading: "var(--el-color-text-heading)",
          muted: "var(--el-color-text-muted)",
          inverse: "var(--el-color-text-inverse)",
        },
        border: {
          subtle: "var(--el-color-border-subtle)",
          DEFAULT: "var(--el-color-border-default)",
          strong: "var(--el-color-border-strong)",
          focus: "var(--el-color-border-focus)",
        },
        status: {
          success: "var(--el-color-success)",
          warning: "var(--el-color-warning)",
          danger: "var(--el-color-danger)",
          info: "var(--el-color-info)",
        },
      },
      borderRadius: {
        sm: "var(--el-radius-sm)",
        md: "var(--el-radius-md)",
        lg: "var(--el-radius-lg)",
        xl: "var(--el-radius-xl)",
        pill: "var(--el-radius-pill)",
      },
      spacing: {
        1: "var(--el-space-1)",
        2: "var(--el-space-2)",
        3: "var(--el-space-3)",
        4: "var(--el-space-4)",
        5: "var(--el-space-5)",
        6: "var(--el-space-6)",
        8: "var(--el-space-8)",
        10: "var(--el-space-10)",
        12: "var(--el-space-12)",
      },
      fontFamily: {
        sans: ["var(--el-font-family-base)"],
        display: ["var(--el-font-family-display)"],
        mono: ["var(--el-font-family-mono)"],
      },
    },
  },
  plugins: [],
};
