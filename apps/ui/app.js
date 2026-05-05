// Implements frontend logic used by the application.
// Author: Thomas Klute

/**
 * Theme switcher - persists in localStorage, default dark.
 * Sets data-theme attribute on <html> for CSS custom property overrides.
 */
const Theme = {
  _key: 'theme',

  init() {
    const saved = localStorage.getItem(this._key);
    this.set(saved || 'dark', false);
  },

  current() {
    return document.documentElement.dataset.theme || 'dark';
  },

  set(name, persist = true) {
    document.documentElement.dataset.theme = name;
    if (persist) localStorage.setItem(this._key, name);
  },
};

/**
 * Kiosk mode - detected via ?kiosk=1 query parameter.
 * Adds body.kiosk class for CSS-driven layout changes.
 */
const Kiosk = {
  active: new URLSearchParams(window.location.search).has('kiosk'),

  init() {
    if (!this.active) return;
    document.body.classList.add('kiosk');
  },
};

/**
 * App router - hash-based page navigation.
 */
const Router = {
  pages: {
    dashboard: DashboardPage,
    recording: RecordingPage,
    detection: DetectionPage,
    streaming: StreamingPage,
    playback: PlaybackPage,
    config: ConfigPage,
  },

  _current: null,
  _replayActive: false,

  init() {
    window.addEventListener('hashchange', () => this.navigate());
    this.navigate();
  },

  navigate() {
    const hash = (location.hash || '#dashboard').slice(1);

    // If replay is active, block navigation to the Recording page.
    if (this._replayActive && hash === 'recording') {
      location.hash = '#dashboard';
      return;
    }

    const page = this.pages[hash] || this.pages.dashboard;
    const container = document.getElementById('page-container');

    // Destroy previous page (stop intervals, etc.)
    if (this._current && this._current.destroy) {
      this._current.destroy();
    }

    // Update nav active state
    document.querySelectorAll('.nav-link').forEach(link => {
      link.classList.toggle('active', link.dataset.page === hash);
    });

    // Render new page
    this._current = page;
    page.render(container);
  },

  // Called by DashboardPage when replay active state changes.
  _updateReplayNav(active) {
    this._replayActive = active;
    const recLink = document.querySelector('.nav-link[data-page="recording"]');
    if (recLink) {
      recLink.style.display = active ? 'none' : '';
    }
    // If the user is currently on the Recording page and replay just started,
    // redirect them to the dashboard.
    if (active) {
      const hash = (location.hash || '#dashboard').slice(1);
      if (hash === 'recording') {
        location.hash = '#dashboard';
      }
    }
  },
};

Theme.init();
Kiosk.init();
Router.init();
