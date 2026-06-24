// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * Recording page - mode selector, naming, start/stop, session list.
 *
 * The preview on this page is the camera_preview (raw live camera feed).
 * Do not conflate with object_detection_preview or field_preview.
 */
const RecordingPage = {
  _interval: null,
  _liveInterval: null,
  _elapsedTimer: null,
  _startTime: null,
  _mode: 'manual', // 'manual' | 'automatic'
  _busy: false, // debounce guard for start/stop
  _page: 0,
  _pageSize: 10,
  _convertingSessionId: null,
  _convertPollTimer: null,

  async render(container) {
    container.innerHTML = `
      <h1>Recording</h1>
      <div class="card">
        <h2>Camera Preview</h2>
        <div class="det-controls">
          <button class="btn btn-primary" id="rec-snap-btn" onclick="RecordingPage.snap()">Snap</button>
          <label class="det-live-toggle">
            <input type="checkbox" id="rec-live-toggle" onchange="RecordingPage.toggleLive(this.checked)">
            <span>Live (~1 fps)</span>
          </label>
        </div>
        <div class="preview-container" id="rec-camera-preview-box">
          <span class="preview-placeholder">Click Snap or enable Live to see the camera preview</span>
        </div>
      </div>
      <div class="card" id="rec-control-card">
        <h2>Recording Control</h2>
        <div class="mode-selector" style="margin-bottom: 0.8rem">
          <label class="mode-option">
            <input type="radio" name="rec-mode" value="manual" checked>
            <span>Manual</span>
          </label>
          <label class="mode-option">
            <input type="radio" name="rec-mode" value="automatic">
            <span>Automatic (GC)</span>
          </label>
        </div>
        <div id="rec-name-field" class="form-group">
          <label for="rec-name-input">Recording name (optional)</label>
          <input type="text" id="rec-name-input" placeholder="e.g. BHuman_vs_HTWK_half1"
                 pattern="[a-zA-Z0-9 _-]*" maxlength="100">
          <span id="rec-name-error" class="field-error" style="display:none">
            Only letters, numbers, spaces, hyphens, and underscores allowed
          </span>
        </div>
        <div id="rec-status" class="loading">Loading...</div>
      </div>
      <div class="card" id="rec-sessions-card">
        <h2>Recordings</h2>
        <div class="loading">Loading...</div>
      </div>
    `;
    this._bindModeSelector();
    this._bindNameInput();
    // Guard against the magnifier widget not being loaded and against
    // the tile being absent.
    if (typeof Magnifier !== 'undefined') {
      const tile = document.getElementById('rec-camera-preview-box');
      if (tile) Magnifier.attach(tile);
    }
    document.addEventListener('click', this._closeMenus);
    await this._loadMode();
    await this.refresh();
    this._interval = setInterval(() => this.refresh(), 3000);
  },

  _bindModeSelector() {
    const radios = document.querySelectorAll('input[name="rec-mode"]');
    radios.forEach(r => {
      r.addEventListener('change', () => this._onModeChange(r.value));
    });
  },

  _bindNameInput() {
    const input = document.getElementById('rec-name-input');
    if (!input) return;
    input.addEventListener('input', () => {
      const valid = /^[a-zA-Z0-9 _-]*$/.test(input.value);
      const errEl = document.getElementById('rec-name-error');
      if (errEl) errEl.style.display = valid ? 'none' : 'block';
      input.classList.toggle('input-error', !valid);
    });
  },

  async _loadMode() {
    try {
      const config = await API.get('/config');
      const mode = config?.video?.recording?.recording_mode || 'manual';
      this._mode = mode;
      const radio = document.querySelector(`input[name="rec-mode"][value="${mode}"]`);
      if (radio) radio.checked = true;
      this._updateModeUI();
    } catch { /* use default */ }
  },

  async _onModeChange(mode) {
    this._mode = mode;
    this._updateModeUI();
    // Save to config
    try {
      const config = await API.get('/config');
      config.video.recording.recording_mode = mode;
      await API.put('/config', config);
    } catch { /* config save failed - mode still applies locally */ }
  },

  _updateModeUI() {
    const nameField = document.getElementById('rec-name-field');
    if (nameField) {
      nameField.style.display = this._mode === 'manual' ? 'block' : 'none';
    }
    this._refreshStatus();
  },

  _closeMenus(e) {
    if (!e.target.closest('.actions-menu')) {
      document.querySelectorAll('.actions-dropdown').forEach(d => d.classList.add('hidden'));
    }
  },

  destroy() {
    clearInterval(this._interval);
    clearInterval(this._elapsedTimer);
    clearInterval(this._convertPollTimer);
    this._stopLive();
    document.removeEventListener('click', this._closeMenus);
    this._interval = null;
    this._elapsedTimer = null;
    this._startTime = null;
    this._convertPollTimer = null;
  },

  async refresh() {
    this._refreshStatus();
    this._refreshSessions();
  },

  async _refreshStatus() {
    try {
      const data = await API.get('/recording/status');
      const el = document.getElementById('rec-status');
      if (!el) return;

      const isAuto = this._mode === 'automatic';

      if (data.recording_active) {
        const label = isAuto ? 'Recording (automatic)' : 'Recording in progress';
        const btnLabel = isAuto ? 'Stop Recording (manual override)' : 'Stop Recording';
        el.innerHTML = `
          <div class="rec-active">
            <span class="rec-dot"></span>
            <strong>${label}</strong>
            <span id="rec-elapsed" class="rec-elapsed"></span>
          </div>
          <div style="margin-top: 0.8rem">
            <button class="btn btn-danger" onclick="RecordingPage.stopRecording()">${btnLabel}</button>
          </div>
        `;
        // Anchor the elapsed counter to the server-reported start time
        // so navigating away and back doesn't reset it. Falls back to
        // Date.now() only when the server didn't supply a timestamp
        // (e.g. an old media-service version).
        const serverStart = data.recording_started_at
          ? Date.parse(data.recording_started_at)
          : null;
        this._startTime = (serverStart && !Number.isNaN(serverStart))
          ? serverStart
          : (this._startTime || Date.now());
        if (!this._elapsedTimer) {
          this._elapsedTimer = setInterval(() => this._updateElapsed(), 1000);
        }
        this._updateElapsed();
      } else {
        this._clearElapsedTimer();
        if (isAuto) {
          el.innerHTML = `
            <div style="margin-bottom: 0.5rem">
              <span class="badge badge-muted">Waiting for GameController</span>
            </div>
            <button class="btn btn-primary" onclick="RecordingPage.startRecording()">Start Recording (manual override)</button>
          `;
        } else {
          el.innerHTML = `
            <div style="margin-bottom: 0.5rem">
              <span class="badge badge-muted">Ready</span>
            </div>
            <button class="btn btn-primary" onclick="RecordingPage.startRecording()">Start Recording</button>
          `;
        }
      }
    } catch { /* handled by API */ }
  },

  _updateElapsed() {
    const el = document.getElementById('rec-elapsed');
    if (!el || !this._startTime) return;
    const secs = Math.floor((Date.now() - this._startTime) / 1000);
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    el.textContent = `${m}:${s.toString().padStart(2, '0')}`;
  },

  _clearElapsedTimer() {
    clearInterval(this._elapsedTimer);
    this._elapsedTimer = null;
    this._startTime = null;
  },

  async _refreshSessions() {
    try {
      const sessions = await API.get('/recording/sessions');
      const el = document.getElementById('rec-sessions-card');
      if (!el) return;

      if (!sessions || sessions.length === 0) {
        el.innerHTML = `
          <h2>Recordings</h2>
          <p style="color: var(--text-muted)">No recordings yet.</p>
        `;
        return;
      }

      const totalPages = Math.ceil(sessions.length / this._pageSize);
      if (this._page >= totalPages) this._page = Math.max(0, totalPages - 1);
      const start = this._page * this._pageSize;
      const pageItems = sessions.slice(start, start + this._pageSize);

      const rows = pageItems.map(s => {
        const name = s.name || '-';
        const date = s.start_time ? new Date(s.start_time).toLocaleString() : '-';
        const duration = s.duration_s ? `${s.duration_s.toFixed(1)}s` : '-';
        const streams = (s.streams || []).map(st => st.stream_type).join(', ') || '-';
        const statusBadge = this._badge(s.status);
        const sid = s.session_id || '';
        const isConverting = this._convertingSessionId === sid;
        const hasMp4 = s.has_mp4 === true;

        // Status column: base badge + MP4 indicator
        let statusCol = statusBadge;
        if (hasMp4) {
          statusCol += ' <span class="badge badge-ok">MP4</span>';
        } else if (isConverting) {
          const pct = this._convertProgress || 0;
          statusCol += ` <span class="badge badge-warn"><span class="convert-spinner"></span> MP4 ${pct}%</span>`;
        }

        // Actions: hide "Convert to MP4" if already converted
        const convertItem = hasMp4 ? '' :
          `<a href="#" onclick="RecordingPage._convertSession('${sid}'); return false;" class="actions-item"${isConverting ? ' disabled' : ''}>Convert to MP4</a>`;
        const replayItem = hasMp4
          ? `<a href="#" onclick="RecordingPage._replaySession('${sid}'); return false;" class="actions-item">Replay</a>`
          : '';
        const actions = `<div class="actions-menu">
          <button class="actions-btn" onclick="RecordingPage._toggleMenu('${sid}')" title="Actions">&#8942;</button>
          <div class="actions-dropdown hidden" id="menu-${sid}">
            ${convertItem}
            ${replayItem}
            <a href="#" onclick="RecordingPage._deleteSession('${sid}'); return false;" class="actions-item actions-danger">Delete</a>
          </div>
        </div>`;
        return `<tr>
          <td>${name}</td>
          <td>${date}</td>
          <td>${duration}</td>
          <td>${streams}</td>
          <td>${statusCol}</td>
          <td>${actions}</td>
        </tr>`;
      }).join('');

      const pager = totalPages > 1 ? `
        <div class="pager">
          <button class="btn btn-secondary btn-sm" onclick="RecordingPage._prevPage()" ${this._page === 0 ? 'disabled' : ''}>Prev</button>
          <span class="pager-info">Page ${this._page + 1} of ${totalPages}</span>
          <button class="btn btn-secondary btn-sm" onclick="RecordingPage._nextPage()" ${this._page >= totalPages - 1 ? 'disabled' : ''}>Next</button>
        </div>` : '';

      el.innerHTML = `
        <h2>Recordings</h2>
        <table>
          <thead><tr><th>Name</th><th>Date</th><th>Duration</th><th>Streams</th><th>Status</th><th>Actions</th></tr></thead>
          <tbody>${rows}</tbody>
        </table>
        ${pager}
      `;
    } catch { /* handled by API */ }
  },

  async snap() {
    const btn = document.getElementById('rec-snap-btn');
    if (btn) { btn.disabled = true; btn.classList.add('btn-busy'); }
    try {
      await this._fetchFrame();
    } finally {
      if (btn) { btn.disabled = false; btn.classList.remove('btn-busy'); }
    }
  },

  toggleLive(enabled) {
    if (enabled) {
      this._fetchFrame();
      this._liveInterval = setInterval(() => this._fetchFrame(), 1000);
    } else {
      this._stopLive();
    }
  },

  _stopLive() {
    clearInterval(this._liveInterval);
    this._liveInterval = null;
    const toggle = document.getElementById('rec-live-toggle');
    if (toggle) toggle.checked = false;
  },

  async _fetchFrame() {
    const box = document.getElementById('rec-camera-preview-box');
    if (!box) return;
    // The media service auto-starts the pipeline on init - there is
    // no separate /camera_preview/start endpoint. The 204-then-retry
    // logic below handles the brief warmup window when the first
    // frame_export buffer hasn't landed yet.
    try {
      let resp = await fetch(`/api/v1/camera_preview/frame?t=${Date.now()}`);
      if (resp.status === 204) {
        await new Promise(r => setTimeout(r, 1000));
        resp = await fetch(`/api/v1/camera_preview/frame?t=${Date.now()}`);
      }
      if (resp.status === 204) {
        box.innerHTML = '<span class="preview-placeholder">No camera preview available - start the pipeline first</span>';
        return;
      }
      if (!resp.ok) throw new Error(resp.statusText);
      const blob = await resp.blob();
      const url = URL.createObjectURL(blob);
      let img = box.querySelector('img');
      if (!img) {
        img = new Image();
        box.innerHTML = '';
        box.appendChild(img);
      }
      const oldUrl = img.src;
      img.src = url;
      if (oldUrl && oldUrl.startsWith('blob:')) URL.revokeObjectURL(oldUrl);
    } catch {
      box.innerHTML = '<span class="preview-placeholder">Failed to fetch camera preview</span>';
    }
  },

  _setButtonsBusy(busy) {
    this._busy = busy;
    document.querySelectorAll('#rec-status .btn').forEach(btn => {
      btn.disabled = busy;
      if (busy) btn.classList.add('btn-busy');
      else btn.classList.remove('btn-busy');
    });
  },

  async startRecording() {
    if (this._busy) return;
    // Validate name input in manual mode
    const body = {};
    if (this._mode === 'manual') {
      const input = document.getElementById('rec-name-input');
      if (input && input.value.trim()) {
        const name = input.value.trim();
        if (!/^[a-zA-Z0-9 _-]*$/.test(name)) {
          Notify.error('Invalid recording name');
          return;
        }
        body.name = name;
      }
    }

    this._setButtonsBusy(true);
    try {
      const result = await API.post('/recording/start', Object.keys(body).length ? body : null);
      if (result.ok) {
        Notify.success('Recording started');
        // _startTime is anchored from the server timestamp inside the
        // next _refreshStatus tick, so don't self-anchor here - would
        // briefly show ~0:00 even when the server reports a started_at
        // a few ms earlier.
      } else {
        Notify.error(result.error || 'Failed to start recording');
      }
      await this._refreshStatus();
    } catch { /* handled by API */ }
    this._setButtonsBusy(false);
  },

  async stopRecording() {
    if (this._busy) return;
    this._setButtonsBusy(true);
    try {
      const result = await API.post('/recording/stop');
      if (result.ok) {
        Notify.success('Recording stopped');
      } else {
        Notify.error(result.error || 'Failed to stop recording');
      }
      this._clearElapsedTimer();
      await this.refresh();
    } catch { /* handled by API */ }
    this._setButtonsBusy(false);
  },

  _toggleMenu(sessionId) {
    // Close all other menus
    document.querySelectorAll('.actions-dropdown').forEach(d => {
      if (d.id !== `menu-${sessionId}`) d.classList.add('hidden');
    });
    const menu = document.getElementById(`menu-${sessionId}`);
    if (menu) menu.classList.toggle('hidden');
  },

  async _deleteSession(sessionId) {
    document.querySelectorAll('.actions-dropdown').forEach(d => d.classList.add('hidden'));
    if (!confirm('Delete this recording session? This cannot be undone.')) return;
    try {
      await API.delete(`/recording/sessions/${sessionId}`);
      Notify.success('Session deleted');
      this._refreshSessions();
    } catch (e) {
      Notify.error('Failed to delete session');
    }
  },

  async _convertSession(sessionId) {
    document.querySelectorAll('.actions-dropdown').forEach(d => d.classList.add('hidden'));
    try {
      const result = await API.post(`/recording/sessions/${sessionId}/convert`);
      if (result.ok) {
        if (result.status === 'already_converted') {
          Notify.success('Already converted to MP4');
        } else {
          this._convertingSessionId = sessionId;
          Notify.success('MP4 conversion started');
          this._startConvertPoll(sessionId);
        }
      } else {
        Notify.error(result.error || 'Failed to start conversion');
      }
    } catch { /* handled by API */ }
    this._refreshSessions();
  },

  async _replaySession(sessionId) {
    document.querySelectorAll('.actions-dropdown').forEach(d => d.classList.add('hidden'));
    try {
      const res = await fetch(`${API.base}/replay/start`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ session_id: sessionId }),
      });
      if (res.ok) {
        Notify.success('Replay started');
        location.hash = '#dashboard';
      } else {
        let detail = res.statusText;
        try {
          const body = await res.json();
          detail = body.detail || body.error || detail;
        } catch { /* ignore parse error */ }
        Notify.error(detail);
      }
    } catch {
      Notify.error('Failed to start replay');
    }
  },

  _startConvertPoll(sessionId) {
    clearInterval(this._convertPollTimer);
    this._convertPollTimer = setInterval(async () => {
      try {
        const status = await API.get(`/recording/sessions/${sessionId}/convert`);
        if (status.status === 'converting') {
          this._convertProgress = status.progress || 0;
          this._refreshSessions();
        } else if (status.status === 'completed') {
          this._convertingSessionId = null;
          clearInterval(this._convertPollTimer);
          this._convertPollTimer = null;
          Notify.success('MP4 conversion complete');
          this._refreshSessions();
        } else if (status.status === 'failed') {
          this._convertingSessionId = null;
          clearInterval(this._convertPollTimer);
          this._convertPollTimer = null;
          Notify.error(status.error || 'Conversion failed');
          this._refreshSessions();
        }
      } catch { /* handled by API */ }
    }, 2000);
  },

  _prevPage() {
    if (this._page > 0) { this._page--; this._refreshSessions(); }
  },

  _nextPage() {
    this._page++;
    this._refreshSessions();
  },

  _badge(status) {
    const cls = {
      completed: 'badge-ok', recording: 'badge-ok',
      failed: 'badge-err', stopping: 'badge-warn',
      created: 'badge-muted',
    }[status] || 'badge-muted';
    return `<span class="badge ${cls}">${status}</span>`;
  },
};
