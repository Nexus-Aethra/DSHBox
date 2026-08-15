import { defineConfig } from 'tsdown'

export default defineConfig({
  entry: ['src/index.ts', 'src/invariant.ts'],
  format: ['esm'],
  dts: { dir: 'lib/types' },
  clean: true,
  target: 'node20',
  platform: 'node',
})
