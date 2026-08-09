module.exports = {
  parserPreset: {
    parserOpts: {
      headerPattern: /^(\w+)(?:\(([^)]+)\))?(!)?:\s(.*)$/,
      headerCorrespondence: ["type", "scope", "breaking", "subject"],
    },
  },
  plugins: [
    {
      rules: {
        "scope-empty-except-ci": (parsed) => {
          const { type, scope } = parsed;
          
          // Allow empty scope when type is 'ci'
          if (type === "ci") {
            return [true];
          }
          
          // Require scope for all other types
          const hasScope = scope && scope.trim().length > 0;
          return [
            hasScope,
            `Scope is required for commits of type '${type}'`,
          ];
        },
      },
    },
  ],
  rules: {
    "type-enum": [
      2,
      "always",
      ["feat", "fix", "docs", "refactor", "perf", "test", "build", "ci", "chore", "revert"],
    ],
    "scope-enum": [
      2,
      "always",
      ["api", "plugins", "cli", "wasm-abi", "wasm-sdk", "wasm-shell", "sdk", "docs", "schema", "nix", "ci"],
    ],
    "scope-empty": [0], // Disable built-in rule
    "scope-empty-except-ci": [2, "always"], // Enable custom rule
    "header-max-length": [2, "always", 72],
    "subject-case": [2, "never", ["sentence-case", "start-case", "pascal-case", "upper-case"]],
  },
};
