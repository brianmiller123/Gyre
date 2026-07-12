import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App'
import './index.css'

// Bundled fonts (no network at runtime): Sora (display) + Plus Jakarta Sans
// (body) + JetBrains Mono (data). Only the weights actually used are imported.
import '@fontsource/sora/400.css'
import '@fontsource/sora/500.css'
import '@fontsource/sora/600.css'
import '@fontsource/sora/700.css'
import '@fontsource/sora/800.css'
import '@fontsource/plus-jakarta-sans/400.css'
import '@fontsource/plus-jakarta-sans/500.css'
import '@fontsource/plus-jakarta-sans/600.css'
import '@fontsource/plus-jakarta-sans/700.css'
import '@fontsource/jetbrains-mono/400.css'
import '@fontsource/jetbrains-mono/500.css'
import '@fontsource/jetbrains-mono/600.css'

// Agent WebUI entry point. Single-page app (no router) so it can be served
// as static files by `agent --serve` without server-side route fallbacks.
// Theme is applied pre-paint in index.html to avoid a flash of the wrong theme.
ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
