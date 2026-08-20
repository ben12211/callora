import eslint from '@eslint/js';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  { ignores: ['dist/**', 'coverage/**', 'node_modules/**'] },
  eslint.configs.recommended,
  ...tseslint.configs.recommended,
  {
    // Maintenance scripts are plain Node ESM, so they use Node's globals directly.
    files: ['scripts/**/*.mjs'],
    languageOptions: {
      globals: { process: 'readonly', Buffer: 'readonly', console: 'readonly' },
    },
  },
  {
    files: ['**/*.ts'],
    rules: {
      '@typescript-eslint/consistent-type-imports': 'error',
      '@typescript-eslint/no-explicit-any': 'error'
    }
  },
);
