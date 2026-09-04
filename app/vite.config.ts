import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// The console is published under the repository's GitHub Pages site, next to the
// coverage report, so production builds need a sub-path base. CI sets BASE_PATH;
// local `npm run dev` / `npm run build` keep serving from the root.
export default defineConfig({
  base: process.env.BASE_PATH ?? '/',
  plugins: [react()],
})
