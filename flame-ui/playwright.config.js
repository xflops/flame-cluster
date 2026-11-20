import { defineConfig } from '@playwright/test';
const liveURL = process.env.FLAME_E2E_URL;
export default defineConfig({
  testDir: './tests', testMatch: '*.spec.js', fullyParallel: true,
  use: { baseURL: liveURL || 'http://127.0.0.1:4173', ...(process.env.PLAYWRIGHT_CHANNEL ? { channel: process.env.PLAYWRIGHT_CHANNEL } : {}), trace: 'retain-on-failure' },
  webServer: liveURL ? undefined : { command: 'npm run build && UI_STATIC_DIR=dist npm run dev', url: 'http://127.0.0.1:4173', reuseExistingServer: !process.env.CI },
});
