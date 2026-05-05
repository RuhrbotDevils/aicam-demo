// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * Streaming page - controls, status, and a canvas-composited
 * preview showing what is (or would be) streamed, including
 * a GameController overlay drawn on top of the
 * camera preview frame.
 *
 * The preview is composited in the browser, NOT via a GStreamer
 * pipeline branch. The canvas draws the raw camera JPEG from
 * `/api/v1/camera_preview/frame` and overlays the GC annotations in
 * JavaScript.
 */
const StreamingPage = {
  _pollInterval: null,
  _autoRefresh: false,
  _refreshTimer: null,
  _teamsMap: {},

  async render(container) {
    container.innerHTML = `
      <h1>Streaming</h1>

      <div class="card">
        <h2>Status &amp; Control</h2>
        <div class="streaming-status" id="streaming-status">
          <span class="streaming-badge idle" id="streaming-badge">Idle</span>
        </div>
        <div class="form-group" style="margin-top:0.5rem">
          <label>Platform</label>
          <span id="streaming-platform">RTMP</span>
        </div>
        <div class="form-group">
          <label>URL</label>
          <span id="streaming-url" class="stats-muted">-</span>
        </div>
        <div style="display:flex;gap:0.5rem;margin-top:0.8rem">
          <button class="btn btn-primary" id="streaming-start-btn"
                  onclick="StreamingPage.start()" disabled>Start Streaming</button>
          <button class="btn btn-danger" id="streaming-stop-btn"
                  onclick="StreamingPage.stop()" disabled>Stop Streaming</button>
        </div>
        <div class="stats-muted" id="streaming-disabled-reason"
             style="margin-top:0.4rem;font-size:0.8rem"></div>
      </div>

      <div class="card">
        <h2>Preview</h2>
        <div class="streaming-toolbar">
          <button class="btn btn-secondary btn-sm" id="streaming-snap-btn"
                  onclick="StreamingPage.snap()">Snap</button>
          <label class="streaming-auto-label">
            <input type="checkbox" id="streaming-auto-cb"
                   onchange="StreamingPage.toggleAutoRefresh(this.checked)">
            Auto-refresh 1 Hz
          </label>
          <span class="stats-muted" id="streaming-gc-source"></span>
        </div>
        <div class="streaming-canvas-box" id="streaming-canvas-box">
          <canvas id="streaming-canvas" width="960" height="540"></canvas>
          <span class="preview-placeholder" id="streaming-placeholder">
            Click Snap or enable Auto-refresh to see the streaming preview
          </span>
        </div>
      </div>

      <div class="card">
        <h2>Overlay</h2>
        <div class="form-group">
          <label>Field name</label>
          <input id="streaming-field-name" class="input" style="max-width:200px"
                 value="FIELD A" placeholder="FIELD A">
        </div>
        <button class="btn btn-secondary btn-sm" onclick="StreamingPage.saveOverlay()">
          Update Field Name
        </button>
        <p class="stats-muted" style="margin-top:0.5rem">
          Overlay data comes from the GameController. Only the field name is configurable.
        </p>
      </div>
    `;

    await this._loadTeams();
    this._refreshStatus();
    this._refreshOverlay();
    this._setPollRate(3000);
  },

  _setPollRate(intervalMs) {
    if (this._pollInterval) clearInterval(this._pollInterval);
    this._currentPollMs = intervalMs;
    this._pollInterval = setInterval(() => this._refreshStatus(), intervalMs);
  },

  destroy() {
    if (this._pollInterval) clearInterval(this._pollInterval);
    if (this._refreshTimer) clearInterval(this._refreshTimer);
    this._pollInterval = null;
    this._refreshTimer = null;
    this._autoRefresh = false;
  },

  // ------------------------------------------------------------------
  // Teams map
  // ------------------------------------------------------------------

  async _loadTeams() {
    try {
      const resp = await fetch('/api/v1/streaming/overlay');
      if (resp.ok) {
        const data = await resp.json();
        // Overlay state has team names already resolved, but we also
        // load the raw teams.json for the browser-side preview
        if (data.field_name) {
          const el = document.getElementById('streaming-field-name');
          if (el) el.value = data.field_name;
        }
      }
    } catch { /* best-effort */ }
    // Load teams.json for browser-side name resolution
    try {
      const resp = await fetch('/config/teams.json');
      if (resp.ok) {
        this._teamsMap = await resp.json();
      }
    } catch { /* fallback to empty map */ }
  },

  _resolveTeam(number) {
    const name = this._teamsMap[String(number)];
    return name || (number ? `Team ${number}` : 'Team ?');
  },

  // ------------------------------------------------------------------
  // Status
  // ------------------------------------------------------------------

  async _refreshStatus() {
    const badge = document.getElementById('streaming-badge');
    const platformEl = document.getElementById('streaming-platform');
    const urlEl = document.getElementById('streaming-url');

    let sc = null;
    try {
      const cfg = await API.get('/config');
      if (cfg && cfg.video && cfg.video.streaming) {
        sc = cfg.video.streaming;
        if (platformEl) platformEl.textContent = sc.platform || 'RTMP';
      }
    } catch { /* best-effort */ }

    let live = false;
    try {
      const s = await API.get('/streaming/status');
      if (urlEl) urlEl.textContent = s.rtmp_url_masked || '-';
      if (s.streaming_error) {
        if (badge) {
          badge.textContent = 'Error: ' + s.streaming_error;
          badge.className = 'streaming-badge error';
        }
      } else if (s.streaming_enabled) {
        live = true;
        if (badge) {
          const fps = s.streaming_fps != null ? ` (${s.streaming_fps.toFixed(1)} fps)` : '';
          badge.textContent = 'Streaming' + fps;
          badge.className = 'streaming-badge active';
        }
      } else if (badge) {
        badge.textContent = 'Idle';
        badge.className = 'streaming-badge idle';
      }
    } catch { /* best-effort */ }

    this._updateButtons(sc, live);

    // Tick faster while streaming so fps / state changes show within
    // ~1 s; back off to 3 s when idle.
    const wantMs = live ? 1000 : 3000;
    if (this._currentPollMs !== wantMs) {
      this._setPollRate(wantMs);
    }
  },

  // Disable Start when streaming is administratively disabled, the
  // RTMP URL is missing, the stream key is missing, or a stream is
  // already running. Disable Stop when no stream is running. Surface
  // the specific reason underneath the buttons so the user knows what
  // to fix in the Config page.
  _updateButtons(sc, live) {
    const startBtn = document.getElementById('streaming-start-btn');
    const stopBtn = document.getElementById('streaming-stop-btn');
    const reasonEl = document.getElementById('streaming-disabled-reason');

    let startReason = null;
    if (!sc) {
      startReason = 'Loading config…';
    } else if (sc.enabled === false) {
      startReason = 'Streaming is disabled in Config (Streaming → Streaming Enabled)';
    } else if (!sc.rtmp_url || !String(sc.rtmp_url).trim()) {
      startReason = 'RTMP URL is empty - set it in Config (Streaming → RTMP URL)';
    } else if (!sc.stream_key || !String(sc.stream_key).trim()) {
      startReason = 'Stream key is empty - set it in Config (Streaming → Stream Key)';
    } else if (live) {
      startReason = 'Already streaming';
    }

    if (startBtn) startBtn.disabled = startReason !== null;
    if (stopBtn) stopBtn.disabled = !live;
    if (reasonEl) {
      // Don't echo "Already streaming" - the badge already says that.
      reasonEl.textContent = (startReason && !live) ? startReason : '';
    }
  },

  // ------------------------------------------------------------------
  // Streaming control
  // ------------------------------------------------------------------

  async start() {
    try {
      await API.post('/streaming/start');
      Notify.success('Streaming started');
    } catch (e) {
      Notify.error('Failed to start streaming: ' + e.message);
    }
    this._refreshStatus();
  },

  async stop() {
    try {
      await API.post('/streaming/stop');
      Notify.success('Streaming stopped');
    } catch (e) {
      Notify.error('Failed to stop streaming: ' + e.message);
    }
    this._refreshStatus();
  },

  // ------------------------------------------------------------------
  // Preview (canvas-composited)
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
    const canvas = document.getElementById('streaming-canvas');
    const placeholder = document.getElementById('streaming-placeholder');
    const gcSourceEl = document.getElementById('streaming-gc-source');
    if (!canvas) return;
    const ctx = canvas.getContext('2d');

    // 1. Fetch the raw camera preview (no detection annotations - the
    //    broadcast overlay is composited on top of clean video).
    let frameBlob;
    try {
      const resp = await fetch(`/api/v1/camera_preview/frame?t=${Date.now()}`);
      if (!resp.ok || resp.status === 204) {
        if (placeholder) placeholder.textContent = 'No preview available - is the pipeline running?';
        return;
      }
      frameBlob = await resp.blob();
    } catch {
      if (placeholder) placeholder.textContent = 'Failed to fetch preview frame';
      return;
    }

    // 2. Fetch the current GC state.
    let gc = null;
    let gcLive = false;
    try {
      const resp = await fetch('/api/v1/game_state/current');
      if (resp.ok && resp.status !== 204) {
        gc = await resp.json();
        gcLive = true;
      }
    } catch { /* no GC data - use dummy below */ }

    // 3. Dummy GC values when no live data.
    if (!gc) {
      gc = {
        team1_number: 0,
        team1_score: 0,
        team2_number: 0,
        team2_score: 0,
        first_half: true,
        secs_remaining: 600,
        state: 'PLAYING',
        set_play: 'NONE',
        kicking_team: 0,
        game_phase: 1,
        packet_number: 0,
      };
    }

    // 4. Draw the frame onto the canvas.
    const img = new Image();
    const url = URL.createObjectURL(frameBlob);
    img.onload = () => {
      canvas.width = img.naturalWidth || 960;
      canvas.height = img.naturalHeight || 540;
      ctx.drawImage(img, 0, 0);
      URL.revokeObjectURL(url);

      // 5. Draw the broadcast overlay.
      this._drawOverlay(ctx, canvas.width, canvas.height, gc, gcLive);

      if (placeholder) placeholder.style.display = 'none';
      canvas.style.display = 'block';
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      if (placeholder) placeholder.textContent = 'Failed to decode preview frame';
    };
    img.src = url;

    if (gcSourceEl) {
      gcSourceEl.textContent = gcLive ? 'GC source: Live' : 'GC source: Dummy';
    }
  },

  // ------------------------------------------------------------------
  // Broadcast overlay drawing (matches cairooverlay in media service)
  // ------------------------------------------------------------------

  _drawOverlay(ctx, w, h, gc, live) {
    const scale = w / 960;
    const margin = 10 * scale;
    const fontSm = 12 * scale;
    const fontMd = 16 * scale;
    const fontLg = 20 * scale;
    const pillPadX = 8 * scale;
    const pillPadY = 4 * scale;
    const pillR = 4 * scale;

    const fieldName = document.getElementById('streaming-field-name')?.value || 'FIELD A';
    const team1 = this._resolveTeam(gc.team1_number);
    const team2 = this._resolveTeam(gc.team2_number);

    // ---- Top-left: field name pill ----
    ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'left';
    const fnM = ctx.measureText(fieldName);
    const fnW = fnM.width + pillPadX * 2;
    const fnH = fontMd + pillPadY * 2;

    ctx.fillStyle = 'rgba(0, 0, 0, 0.7)';
    ctx.beginPath();
    ctx.roundRect(margin, margin, fnW, fnH, pillR);
    ctx.fill();
    ctx.fillStyle = '#ffffff';
    ctx.fillText(fieldName, margin + pillPadX, margin + pillPadY + fontMd * 0.85);

    // ---- Top-right: clock pill ----
    const now = new Date();
    const timeStr = `${String(now.getHours()).padStart(2, '0')}:${String(now.getMinutes()).padStart(2, '0')}:${String(now.getSeconds()).padStart(2, '0')}`;
    const tmM = ctx.measureText(timeStr);
    const tmW = tmM.width + pillPadX * 2;
    const tmH = fnH;

    ctx.fillStyle = 'rgba(0, 0, 0, 0.7)';
    ctx.beginPath();
    ctx.roundRect(w - margin - tmW, margin, tmW, tmH, pillR);
    ctx.fill();
    ctx.fillStyle = '#ffffff';
    ctx.textAlign = 'right';
    ctx.fillText(timeStr, w - margin - pillPadX, margin + pillPadY + fontMd * 0.85);

    // ---- Bottom-center: 3-row scoreboard ----
    this._drawScoreboard(ctx, w, h, gc, live, team1, team2, scale, margin, fontSm, fontMd, fontLg);
  },

  _drawScoreboard(ctx, w, h, gc, live, team1, team2, scale, margin, fontSm, fontMd, fontLg) {
    const rowH = 22 * scale;
    const scoreW = 22 * scale;
    const clockW = 60 * scale;
    const pad = 6 * scale;
    const gap = 2 * scale;
    const r = 3 * scale;

    // Compute name column width
    ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'left';
    const nameW = Math.max(ctx.measureText(team1).width, ctx.measureText(team2).width) + pad * 2;

    const totalW = nameW + scoreW + gap + clockW + gap + scoreW + nameW;
    const sbX = (w - totalW) / 2;
    const sbH = rowH * 3 + pad * 2;
    const sbY = h - margin - sbH;

    // Background
    ctx.fillStyle = 'rgba(0, 0, 0, 0.65)';
    ctx.beginPath();
    ctx.roundRect(sbX - pad, sbY - pad, totalW + pad * 2, sbH + pad, r);
    ctx.fill();

    // --- Row 1: packets + phase ---
    ctx.font = `bold ${fontSm}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.fillStyle = live ? '#cccccc' : '#ffcc66';
    const phase = gc.first_half ? '1st' : '2nd';
    const pkt = String(gc.packet_number || 0);

    ctx.textAlign = 'center';
    ctx.fillText(pkt, sbX + nameW / 2, sbY + fontSm);
    ctx.fillText(phase, w / 2, sbY + fontSm);
    ctx.fillText(pkt, sbX + totalW - nameW / 2, sbY + fontSm);

    // --- Row 2: team1 | score(red) | clock | score(blue) | team2 ---
    const rowY = sbY + rowH;
    const yText = rowY + fontLg * 0.85;

    // Team 1 name (right-aligned)
    ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'right';
    ctx.fillStyle = '#ffffff';
    ctx.fillText(team1, sbX + nameW - pad, yText);

    // Score 1 (red box)
    const xScore1 = sbX + nameW;
    ctx.fillStyle = 'rgba(217, 25, 25, 1)';
    ctx.beginPath();
    ctx.roundRect(xScore1, rowY, scoreW, rowH, r);
    ctx.fill();
    ctx.font = `bold ${fontLg}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'center';
    ctx.fillStyle = '#ffffff';
    ctx.fillText(String(gc.team1_score), xScore1 + scoreW / 2, yText);

    // Clock (dark box)
    const xClock = xScore1 + scoreW + gap;
    ctx.fillStyle = 'rgba(38, 38, 38, 0.9)';
    ctx.beginPath();
    ctx.roundRect(xClock, rowY, clockW, rowH, r);
    ctx.fill();
    const mins = Math.floor(Math.abs(gc.secs_remaining) / 60);
    const secs = Math.abs(gc.secs_remaining) % 60;
    const clockStr = `${String(mins).padStart(2, '0')}:${String(secs).padStart(2, '0')}`;
    ctx.fillStyle = '#ffffff';
    ctx.fillText(clockStr, xClock + clockW / 2, yText);

    // Score 2 (blue box)
    const xScore2 = xClock + clockW + gap;
    ctx.fillStyle = 'rgba(25, 50, 217, 1)';
    ctx.beginPath();
    ctx.roundRect(xScore2, rowY, scoreW, rowH, r);
    ctx.fill();
    ctx.fillStyle = '#ffffff';
    ctx.fillText(String(gc.team2_score), xScore2 + scoreW / 2, yText);

    // Team 2 name (left-aligned)
    ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'left';
    ctx.fillStyle = '#ffffff';
    ctx.fillText(team2, xScore2 + scoreW + pad, yText);

    // --- Row 3: game state ---
    const stateY = sbY + rowH * 2;
    ctx.font = `bold ${fontSm}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'center';
    ctx.fillStyle = live ? '#e0e0e0' : '#ffcc66';
    const stateLine = this._formatStateLine(gc, team1, team2);
    ctx.fillText(stateLine, w / 2, stateY + fontSm + 2 * scale);
  },

  _formatStateLine(gc, team1, team2) {
    const state = (gc.state || 'PLAYING').toLowerCase();
    const setPlay = gc.set_play || 'NONE';
    if (setPlay !== 'NONE') {
      const play = setPlay.toLowerCase().replace(/_/g, ' ');
      const kicking = gc.kicking_team === gc.team1_number ? team1 :
                      gc.kicking_team === gc.team2_number ? team2 : 'unknown';
      return `${state}, ${play} for ${kicking}`;
    }
    if (gc.state === 'READY' || gc.state === 'SET') {
      const kicking = gc.kicking_team === gc.team1_number ? team1 :
                      gc.kicking_team === gc.team2_number ? team2 : 'unknown';
      return `${state}, kickoff for ${kicking}`;
    }
    return state;
  },

  // ------------------------------------------------------------------
  // Overlay config
  // ------------------------------------------------------------------

  async _refreshOverlay() {
    try {
      const data = await API.get('/streaming/overlay');
      const el = document.getElementById('streaming-field-name');
      if (el && data && data.field_name) {
        el.value = data.field_name;
      }
    } catch { /* best-effort */ }
  },

  async saveOverlay() {
    const el = document.getElementById('streaming-field-name');
    if (!el) return;
    try {
      await API.put('/streaming/overlay', { field_name: el.value });
      Notify.success('Field name updated');
    } catch (e) {
      Notify.error('Failed to save overlay: ' + e.message);
    }
  },
};
