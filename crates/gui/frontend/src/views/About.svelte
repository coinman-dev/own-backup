<script lang="ts">
  // Static about page -- no async calls needed
</script>

<div class="about-view">
  <div class="page-header">
    <h1 class="page-title">About</h1>
  </div>

  <div class="about-card card">
    <div class="app-logo">
      <svg width="56" height="56" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
        <rect x="2" y="6" width="20" height="12" rx="2" />
        <path d="M12 6V2M8 6V4M16 6V4" />
        <circle cx="12" cy="12" r="3" />
      </svg>
    </div>

    <h2 class="app-name">My Own Backup</h2>
    <p class="app-version">Version 0.1.0</p>
    <p class="app-description">
      Open-source block-level backup system for disk imaging, incremental backups, and bare-metal restore. Built with Rust.
    </p>

    <div class="info-table">
      <div class="info-row">
        <span class="info-label">License</span>
        <span class="info-value">MIT / Apache-2.0</span>
      </div>
      <div class="info-row">
        <span class="info-label">Image Format</span>
        <span class="info-value">.obk (cross-platform)</span>
      </div>
      <div class="info-row">
        <span class="info-label">Compression</span>
        <span class="info-value">zstd (level 0-19)</span>
      </div>
      <div class="info-row">
        <span class="info-label">Encryption</span>
        <span class="info-value">AES-256-GCM</span>
      </div>
      <div class="info-row">
        <span class="info-label">Backend</span>
        <span class="info-value">gRPC on localhost:50051</span>
      </div>
      <div class="info-row">
        <span class="info-label">Metadata</span>
        <span class="info-value">SQLite</span>
      </div>
      <div class="info-row">
        <span class="info-label">GUI Framework</span>
        <span class="info-value">Tauri 2 + Svelte 5</span>
      </div>
    </div>

    <div class="links-section">
      <h3 class="links-heading">Links</h3>
      <div class="links-list">
        <a href="https://github.com/user/own-backup" target="_blank" rel="noopener" class="link-item">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
            <path d="M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0 0 24 12c0-6.63-5.37-12-12-12z" />
          </svg>
          GitHub Repository
        </a>
        <a href="https://github.com/user/own-backup/issues" target="_blank" rel="noopener" class="link-item">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
            <circle cx="12" cy="12" r="10" />
            <path d="M12 8v4m0 4h.01" />
          </svg>
          Report an Issue
        </a>
        <a href="https://github.com/user/own-backup/wiki" target="_blank" rel="noopener" class="link-item">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
            <path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20" />
            <path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z" />
          </svg>
          Documentation
        </a>
      </div>
    </div>

    <div class="copyright">
      <p>Copyright 2025-2026. All rights reserved.</p>
      <p>Built with Rust, Tauri, and Svelte.</p>
    </div>
  </div>
</div>

<style>
  .about-view {
    padding: 28px 32px;
    height: 100%;
    overflow-y: auto;
  }

  .page-header {
    margin-bottom: 24px;
  }

  .page-title {
    font-size: 24px;
    font-weight: 600;
    color: var(--text);
    margin: 0;
  }

  .about-card {
    max-width: 600px;
    text-align: center;
  }

  .app-logo {
    margin-bottom: 16px;
  }

  .app-name {
    font-size: 22px;
    font-weight: 700;
    color: var(--text);
    margin: 0 0 4px;
  }

  .app-version {
    font-size: 14px;
    color: var(--text-muted);
    margin: 0 0 16px;
  }

  .app-description {
    font-size: 14px;
    color: var(--text-secondary);
    line-height: 1.6;
    margin: 0 0 24px;
    max-width: 480px;
    margin-left: auto;
    margin-right: auto;
  }

  .info-table {
    text-align: left;
    margin-bottom: 24px;
    border-top: 1px solid var(--border);
  }

  .info-row {
    display: flex;
    justify-content: space-between;
    padding: 10px 0;
    border-bottom: 1px solid var(--border);
  }

  .info-label {
    font-size: 13px;
    color: var(--text-muted);
  }

  .info-value {
    font-size: 13px;
    font-weight: 500;
    color: var(--text);
  }

  .links-section {
    text-align: left;
    margin-bottom: 24px;
  }

  .links-heading {
    font-size: 14px;
    font-weight: 600;
    color: var(--text);
    margin: 0 0 10px;
  }

  .links-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .link-item {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 12px;
    border-radius: 6px;
    font-size: 13px;
    color: var(--primary);
    text-decoration: none;
    transition: background-color 0.12s ease;
  }

  .link-item:hover {
    background: rgba(45, 125, 210, 0.06);
    text-decoration: none;
  }

  .copyright {
    font-size: 12px;
    color: var(--text-muted);
    padding-top: 16px;
    border-top: 1px solid var(--border);
  }

  .copyright p {
    margin: 0 0 2px;
  }
</style>
