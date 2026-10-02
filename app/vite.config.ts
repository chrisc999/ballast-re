import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// Relative asset paths, so one build serves correctly wherever the site is mounted:
// app.ballastre.xyz/, the repository's github.io sub-path, or a local preview.
export default defineConfig({
  base: './',
  plugins: [react()],
})
