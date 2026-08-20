import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    // The suite is fast once warm, but the first run of a fresh checkout or a cold CI
    // cache spends seconds transforming before a single test executes, and the 5s default
    // turned that into flaky failures that said nothing about the code.
    testTimeout: 20_000,
    hookTimeout: 20_000,
  },
});
