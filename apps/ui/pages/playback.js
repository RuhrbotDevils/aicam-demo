// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * Playback page - select a previously recorded match from the
 * playback/ directory, choose which half to replay, and
 * start/stop the replay from a single button. 
*/
const PlaybackPage = {
  _pollInterval: null,
  _autoRefresh: false,
  _refreshTimer: null,
  _sessions: [],
  _isActive: false,  // true while a playback or replay is producing frames
  _wasActive: false, // previous _isActive: drives the auto-revert auto-refresh
  _autoRefreshBeforePlayback: null,  // null = no remembered prior state
  _currentSpeed: 1,  // last speed sent on /replay/start or /playback/start

  async render(container) {
    container.innerHTML = `
      <h1>Playback</h1>

      <div class="card">
        <h2>Session</h2>
        <div class="form-group">
          <label>Session</label>
          <select id="pb-session" class="input" onchange="PlaybackPage.onSessionChange()">
            <option value="">Loading...</option>
          </select>
        </div>
        <div class="form-group">
          <label>Half</label>
          <select id="pb-half" class="input">
            <option value="1">Half 1</option>
          </select>
        </div>
        <div class="form-group">
          <label>Speed</label>
          <select id="pb-speed" class="input">
            <option value="0.25">0.25x</option>
            <option value="0.5">0.5x</option>
            <option value="1" selected>1x (realtime)</option>
            <option value="2">2x</option>
            <option value="4">4x</option>
            <option value="0">Max</option>
          </select>
        </div>
      </div>

      <div class="card">
        <h2>Control</h2>
        <div class="playback-status">
          <span class="playback-badge idle" id="pb-badge">Idle</span>
          <span class="stats-muted" id="pb-progress"></span>
        </div>
        <div style="display:flex;gap:0.5rem;margin-top:0.8rem">
          <button class="btn btn-primary" id="pb-start-btn" onclick="PlaybackPage.start()" disabled>&#9654; Start</button>
          <button class="btn btn-danger" id="pb-stop-btn" onclick="PlaybackPage.stop()" disabled>&#9632; Stop</button>
        </div>
      </div>

      <div class="card">
        <h2>Preview</h2>
        <div class="streaming-toolbar">
          <button class="btn btn-secondary btn-sm" id="pb-snap-btn"
                  onclick="PlaybackPage.snap()" disabled>Snap</button>
          <label class="streaming-auto-label">
            <input type="checkbox" id="pb-auto-cb" disabled
                   onchange="PlaybackPage.toggleAutoRefresh(this.checked)">
            Auto-refresh 1 Hz
          </label>
        </div>
        <div class="streaming-canvas-box" id="pb-canvas-box">
          <canvas id="pb-canvas" width="960" height="540"></canvas>
          <span class="preview-placeholder" id="pb-placeholder">
            Start a replay and click Snap or enable Auto-refresh
          </span>
        </div>
      </div>
    `;

    await this._loadSessions();
    this._refreshStatus();
    this._pollInterval = setInterval(() => this._refreshStatus(), 2000);
  },

  destroy() {
    if (this._pollInterval) clearInterval(this._pollInterval);
    if (this._refreshTimer) clearInterval(this._refreshTimer);
    this._pollInterval = null;
    this._refreshTimer = null;
    this._autoRefresh = false;
  },

  // ------------------------------------------------------------------
  // Sessions
  // ------------------------------------------------------------------

  async _loadSessions() {
    // Two source lists merged into one dropdown:
    //   1. /playback/sessions - Phase-1 Python video-replay sessions
    //      (entries from playback/<dir>/playback.yaml).
    //   2. /recording/sessions - recorded sessions where the MP4 has
    //      been converted (has_mp4=true). Marked with `_kind = 'rec'`
    //      and a " (rec)" suffix on the display name so the operator
    //      can tell them apart from plain video-replay sessions.
    let pbSessions = [];
    let recSessions = [];
    try { pbSessions = await API.get('/playback/sessions'); } catch { pbSessions = []; }
    try {
      const recs = await API.get('/recording/sessions');
      recSessions = (recs || [])
        .filter(r => r.has_mp4)
        .map(r => ({
          _kind: 'rec',
          dir_name: r.session_id,
          // session.json carries `name` as the operator's chosen
          // recording label (nullable). Fall back to session_id.
          name: `${r.name || r.session_id} (rec)`,
          has_half2: false,
        }));
    } catch { recSessions = []; }
    pbSessions = (pbSessions || []).map(s => ({ ...s, _kind: 'playback' }));
    this._sessions = [...pbSessions, ...recSessions];

    const sel = document.getElementById('pb-session');
    if (!sel) return;
    if (this._sessions.length === 0) {
      sel.innerHTML = '<option value="">No sessions available</option>';
      return;
    }
    sel.innerHTML = this._sessions.map(s =>
      `<option value="${this._escape(s.dir_name)}">${this._escape(s.name)}</option>`
    ).join('');
    this.onSessionChange();
  },

  onSessionChange() {
    const sel = document.getElementById('pb-session');
    const halfSel = document.getElementById('pb-half');
    const halfRow = halfSel ? halfSel.closest('.form-group') : null;
    if (!sel || !halfSel) return;
    const dir = sel.value;
    const session = this._sessions.find(s => s.dir_name === dir);
    // Refresh button gating since the session selection changed.
    this._applyButtonGating();
    if (!session) return;
    // Recording-replay (MP4) doesn't carry a half concept; hide that row.
    if (halfRow) halfRow.style.display = (session._kind === 'rec') ? 'none' : '';
    if (session.has_half2) {
      halfSel.innerHTML = '<option value="1">Half 1</option><option value="2">Half 2</option>';
    } else {
      halfSel.innerHTML = '<option value="1">Half 1</option>';
    }
  },

  /** Has the operator picked a real session in the dropdown? */
  _hasValidSession() {
    const sel = document.getElementById('pb-session');
    if (!sel || !sel.value) return false;
    return this._sessions.some(s => s.dir_name === sel.value);
  },

  /** Disable Start unless idle AND a session is selected; disable Stop unless playing. */
  _applyButtonGating() {
    const startBtn = document.getElementById('pb-start-btn');
    const stopBtn = document.getElementById('pb-stop-btn');
    if (startBtn) startBtn.disabled = this._isActive || !this._hasValidSession();
    if (stopBtn) stopBtn.disabled = !this._isActive;
  },

  // ------------------------------------------------------------------
  // Control
  // ------------------------------------------------------------------

  async start() {
    const sel = document.getElementById('pb-session');
    const session = sel?.value;
    if (!session) { Notify.error('No session selected'); return; }
    const entry = this._sessions.find(s => s.dir_name === session);
    const isRec = entry && entry._kind === 'rec';
    let started = false;
    try {
      const speed = parseFloat(document.getElementById('pb-speed')?.value || '1');
      this._currentSpeed = speed;
      if (isRec) {
        // Converted recording -> media-service replay (GStreamer MP4).
        await API.post('/replay/start', { session_id: session, speed });
      } else {
        const half = parseInt(document.getElementById('pb-half')?.value || '1');
        await API.post('/playback/start', { session, half, speed });
      }
      started = true;
      Notify.success('Playback started');
    } catch (e) {
      Notify.error('Failed to start playback: ' + e.message);
    }
    if (started) {
      this._autoRefreshBeforePlayback = this._autoRefresh;
      this._isActive = true;
      this._wasActive = true;
      const autoCb = document.getElementById('pb-auto-cb');
      if (autoCb) {
        autoCb.disabled = false;
        autoCb.checked = true;
      }
      this.toggleAutoRefresh(true);
    }
    this._refreshStatus();
  },

  async stop() {
    // Try both stops - exactly one will be active. Errors are silent
    // because the inactive endpoint may legitimately 4xx.
    const stops = [
      API.post('/playback/stop').catch(() => null),
      API.post('/replay/stop').catch(() => null),
    ];
    await Promise.all(stops);
    this._refreshStatus();
  },

  // ------------------------------------------------------------------
  // Status
  // ------------------------------------------------------------------

  async _refreshStatus() {
    try {
      const [pb, rep] = await Promise.all([
        API.get('/playback/status').catch(() => ({})),
        API.get('/replay/status').catch(() => ({})),
      ]);
      const badge = document.getElementById('pb-badge');
      const progress = document.getElementById('pb-progress');
      if (!badge) return;

      const pbPlaying = pb && pb.state === 'playing';
      const repPlaying = rep && rep.active === true;
      const pbDone = pb && pb.state === 'done';

      if (pbPlaying) {
        badge.textContent = 'Playing';
        badge.className = 'playback-badge playing';
        if (progress && pb.total_frames > 0) {
          const pct = ((pb.frames_published / pb.total_frames) * 100).toFixed(1);
          const speedTag = this._formatSpeedTag();
          progress.textContent = `frame ${pb.frames_published} / ${pb.total_frames} (${pct}%)${speedTag}`;
        }
        this._isActive = true;
      } else if (repPlaying) {
        badge.textContent = 'Playing (recording)';
        badge.className = 'playback-badge playing';
        if (progress) {
          // position_s and duration_s are media-time. With speed != 1
          // the wall-clock pacing differs from the media time, so we
          // append the speed tag - e.g. "5.5s / 68.2s (8%) @ 2x" - to
          // make the rate explicit. "max" speed has unbounded rate;
          // we just label it as such.
          const pos = (rep.position_s !== undefined) ? rep.position_s.toFixed(1) : '?';
          const dur = (rep.duration_s !== undefined && rep.duration_s > 0) ? rep.duration_s.toFixed(1) : '?';
          let pctStr = '';
          if (rep.duration_s && rep.duration_s > 0 && rep.position_s !== undefined) {
            const pct = (rep.position_s / rep.duration_s) * 100;
            pctStr = ` (${pct.toFixed(0)}%)`;
          }
          const speedTag = this._formatSpeedTag();
          progress.textContent = `${pos}s / ${dur}s${pctStr}${speedTag}`;
        }
        this._isActive = true;
      } else if (pbDone) {
        badge.textContent = 'Done';
        badge.className = 'playback-badge done';
        if (progress) progress.textContent = 'Replay complete';
        this._isActive = false;
      } else {
        badge.textContent = 'Idle';
        badge.className = 'playback-badge idle';
        if (progress) progress.textContent = '';
        this._isActive = false;
      }

      // Active → not active transition: restore the auto-refresh
      // state we captured on start(). Catches both manual Stop and
      // natural EOS (replay returns to idle when the file ends).
      if (this._wasActive && !this._isActive) {
        this._restoreAutoRefreshAfterPlayback();
      }
      this._wasActive = this._isActive;

      this._applyPreviewControls();
      this._applyButtonGating();
    } catch { /* best-effort */ }
  },

  /** Append " @ 2x" / " @ max" to progress strings so the rate is explicit. */
  _formatSpeedTag() {
    const s = this._currentSpeed;
    if (s == null) return '';
    if (s === 0) return ' @ max';
    if (s === 1) return '';
    // Drop trailing .0 so "0.5x" stays clean; keep one decimal otherwise.
    const txt = Number.isInteger(s) ? `${s}x` : `${s}x`;
    return ` @ ${txt}`;
  },

  _restoreAutoRefreshAfterPlayback() {
    if (this._autoRefreshBeforePlayback === null) return;
    const restoreTo = this._autoRefreshBeforePlayback;
    this._autoRefreshBeforePlayback = null;
    const autoCb = document.getElementById('pb-auto-cb');
    if (autoCb) autoCb.checked = restoreTo;
    this.toggleAutoRefresh(restoreTo);
  },

  /** Enable / disable the Snap button + auto-refresh checkbox.
   *
   * When no replay is producing frames, disable both so the
   * operator can't fetch a stale preview. Also stops a running
   * auto-refresh so its 1 Hz timer doesn't keep ticking against
   * an idle backend.
   */
  _applyPreviewControls() {
    const snapBtn = document.getElementById('pb-snap-btn');
    const autoCb = document.getElementById('pb-auto-cb');
    const active = this._isActive;
    if (snapBtn) snapBtn.disabled = !active;
    if (autoCb) autoCb.disabled = !active;
    if (!active && this._autoRefresh) {
      this._autoRefresh = false;
      if (this._refreshTimer) clearInterval(this._refreshTimer);
      this._refreshTimer = null;
      if (autoCb) autoCb.checked = false;
    }
  },

  // ------------------------------------------------------------------
  // Preview (reuses StreamingPage canvas pattern)
  // ------------------------------------------------------------------

  toggleAutoRefresh(on) {
    this._autoRefresh = on;
    if (this._refreshTimer) clearInterval(this._refreshTimer);
    this._refreshTimer = null;
    if (on) {
      this.snap();
      this._refreshTimer = setInterval(() => this.snap(), 1000);
    }
  },

  async snap() {
    const canvas = document.getElementById('pb-canvas');
    const placeholder = document.getElementById('pb-placeholder');
    if (!canvas) return;
    const ctx = canvas.getContext('2d');

    // Fetch the raw camera preview frame. The detection preview
    // (object_detection_preview) is rendered at the AI model's input
    // resolution (e.g. 1408x800) and would force the canvas into the
    // model's aspect ratio - irrelevant here, since playback shows
    // the source video, not detections.
    let frameBlob;
    try {
      const resp = await fetch(`/api/v1/camera_preview/frame?t=${Date.now()}`);
      if (!resp.ok || resp.status === 204) {
        if (placeholder) {
          placeholder.textContent = 'Preview not available - start a replay first.';
          placeholder.style.display = '';
        }
        return;
      }
      frameBlob = await resp.blob();
    } catch {
      if (placeholder) placeholder.textContent = 'Failed to fetch preview';
      return;
    }

    const img = new Image();
    const url = URL.createObjectURL(frameBlob);
    img.onload = () => {
      canvas.width = img.naturalWidth || 960;
      canvas.height = img.naturalHeight || 540;
      ctx.drawImage(img, 0, 0);
      URL.revokeObjectURL(url);
      // Playback shows the raw replayed frame only - the streaming-style
      // scoreboard / GC overlay is not drawn here.
      if (placeholder) placeholder.style.display = 'none';
      canvas.style.display = 'block';
    };
    img.onerror = () => { URL.revokeObjectURL(url); };
    img.src = url;
  },

  _escape(s) {
    const d = document.createElement('div');
    d.textContent = s;
    return d.innerHTML;
  },
};
