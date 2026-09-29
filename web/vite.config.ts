import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// `npm run dev` talks to a local server (`./dev up`) through the proxy; CALLORA_API points it
// elsewhere (another port, or a stand-in API).
export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: { proxy: { "/api": process.env.CALLORA_API ?? "http://localhost:3000" } },
  build: { outDir: "dist", sourcemap: false },
});
