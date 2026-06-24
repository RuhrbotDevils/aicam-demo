// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * Streaming page - controls, status, and a canvas-composited
 * preview showing what is (or would be) streamed, including
 * a broadcast-quality GameController overlay drawn on top of the
 * camera preview frame.
 *
 * The preview is composited in the browser, NOT via a GStreamer
 * pipeline branch. The canvas draws the raw camera JPEG from
 * /api/v1/camera_preview/frame (the same endpoint the Dashboard
 * uses - sourced from the frame_export tee branch's tmpfs, not
 * the AI/object-detection branch) and overlays the GC annotations
 * in JavaScript.
 *
 * Layout matches the cairooverlay in the Rust media service.
 */
const StreamingPage = {
  _pollInterval: null,
  _autoRefresh: false,
  _refreshTimer: null,
  _teamsMap: {},
  // Track the current poll cadence so the rebind only fires on
  // transition (3 s while idle, 1 s while streaming). 1 s makes fps
  // and any rtmpsink error visible within a tick; 3 s reduces idle
  // traffic.
  _pollIntervalMs: 0,
  _IDLE_POLL_MS: 3000,
  _LIVE_POLL_MS: 1000,

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
                  onclick="StreamingPage.start()">Start Streaming</button>
          <button class="btn btn-danger" id="streaming-stop-btn"
                  onclick="StreamingPage.stop()">Stop Streaming</button>
        </div>
        <div class="stats-muted" id="streaming-button-hint" style="margin-top:0.4rem"></div>
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
    this._setPollRate(this._IDLE_POLL_MS);
  },

  /**
   * Rebind the status-poll interval at the requested rate. No-op when
   * already at that rate so the active poll keeps its phase across
   * refreshStatus calls.
   */
  _setPollRate(intervalMs) {
    if (this._pollIntervalMs === intervalMs) return;
    if (this._pollInterval) clearInterval(this._pollInterval);
    this._pollInterval = setInterval(() => this._refreshStatus(), intervalMs);
    this._pollIntervalMs = intervalMs;
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
        if (data.field_name) {
          const el = document.getElementById('streaming-field-name');
          if (el) el.value = data.field_name;
        }
      }
    } catch { /* best-effort */ }
    // Use the /api/v1/teams endpoint instead of fetching
    // /config/teams.json directly. The endpoint normalises both the
    // legacy plain-string schema and the new object-form schema
    // (which carries jersey colours alongside the name), so this JS
    // never has to re-implement the schema parse.
    try {
      const resp = await fetch('/api/v1/teams');
      if (resp.ok) {
        const arr = await resp.json();
        const map = {};
        for (const t of arr) {
          map[String(t.number)] = { name: t.name, color: t.color || null };
        }
        this._teamsMap = map;
      }
    } catch { /* fallback to empty map */ }
  },

  _resolveTeam(number) {
    const entry = this._teamsMap[String(number)];
    if (entry && typeof entry === 'object' && entry.name) return entry.name;
    if (typeof entry === 'string') return entry;  // legacy callers
    return number ? `Team ${number}` : 'Team ?';
  },

  /**
   * Resolve a team number to a CSS colour string. Falls
   * back to ``fallback`` when no colour is configured. Accepts the
   * #RRGGBB form returned by /api/v1/teams (mirrors the Rust
   * `resolve_team_color` helper).
   */
  _resolveTeamColor(number, fallback) {
    const entry = this._teamsMap[String(number)];
    if (entry && typeof entry === 'object' && entry.color) return entry.color;
    return fallback;
  },

  /**
   * Pick `#000000` or `#ffffff` depending on which gives
   * better contrast against `bg`. Mirrors the Rust
   * `contrasting_text_rgb` / `Rgba::contrasting_text` helpers in
   * the cairo and NV12 overlay paths so the three rendering
   * surfaces stay visually consistent. Accepts `#rrggbb`, `#rgb`,
   * or `rgb(r, g, b)` strings. Unknown formats fall back to white.
   */
  _contrastingTextColor(bg) {
    if (!bg || typeof bg !== 'string') return '#ffffff';
    let r, g, b;
    const rgbM = /rgb\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)/i.exec(bg);
    if (rgbM) {
      r = +rgbM[1]; g = +rgbM[2]; b = +rgbM[3];
    } else {
      const hex6 = /^#([0-9a-f]{6})$/i.exec(bg);
      const hex3 = /^#([0-9a-f]{3})$/i.exec(bg);
      if (hex6) {
        r = parseInt(hex6[1].slice(0, 2), 16);
        g = parseInt(hex6[1].slice(2, 4), 16);
        b = parseInt(hex6[1].slice(4, 6), 16);
      } else if (hex3) {
        r = parseInt(hex3[1][0] + hex3[1][0], 16);
        g = parseInt(hex3[1][1] + hex3[1][1], 16);
        b = parseInt(hex3[1][2] + hex3[1][2], 16);
      } else {
        return '#ffffff';
      }
    }
    // ITU-R BT.601 perceptual luminance. Threshold 0.6 leans
    // toward white text to preserve the broadcast "white on dark
    // jersey" look (red / blue / dark green) and only flips to
    // black for genuinely bright backgrounds.
    const l = (0.299 * r + 0.587 * g + 0.114 * b) / 255;
    return l > 0.6 ? '#000000' : '#ffffff';
  },

  /**
   * Mirrors Rust `team_colour_byte_to_rgb` in overlay.rs: map the
   * HSL TeamInfo colour-enum byte (0..9) to a CSS rgb()
   * string. Anything outside the spec returns null so callers fall
   * through to their configured/fallback colour.
   */
  _teamColourByteToCSS(byte) {
    const table = {
      0: 'rgb(26, 102, 230)',   // BLUE
      1: 'rgb(217, 38, 38)',    // RED
      2: 'rgb(242, 217, 38)',   // YELLOW
      3: 'rgb(26, 26, 26)',     // BLACK
      4: 'rgb(242, 242, 242)',  // WHITE
      5: 'rgb(38, 179, 77)',    // GREEN
      6: 'rgb(242, 140, 26)',   // ORANGE
      7: 'rgb(140, 51, 179)',   // PURPLE
      8: 'rgb(115, 77, 38)',    // BROWN
      9: 'rgb(140, 140, 140)',  // GRAY
    };
    return Object.prototype.hasOwnProperty.call(table, byte)
      ? table[byte]
      : null;
  },

  /**
   * Resolve the goalkeeper colour for a team. Returns a
   * CSS rgb() string when the goalkeeper byte is present AND differs
   * from the field-player byte. Mirrors the Rust suppression-when-
   * equal logic in `parse_game_state`: two same-coloured strips
   * would read as one wider block, so we surface them only when
   * they actually distinguish the goalkeeper.
   */
  _resolveGoalkeeperColor(gkByte, fieldByte) {
    if (gkByte === null || gkByte === undefined) return null;
    if (gkByte === fieldByte) return null;
    return this._teamColourByteToCSS(gkByte);
  },

  // ------------------------------------------------------------------
  // Status
  // ------------------------------------------------------------------

  async _refreshStatus() {
    try {
      // Hit the dedicated endpoint instead of the generic /status
      // (which doesn't carry streaming-specific fields). Drives a
      // small Idle / Streaming / Error state machine on the badge
      // based on what the media service actually reports.
      const st = await API.get('/streaming/status');
      const badge = document.getElementById('streaming-badge');
      if (badge && st) {
        let label;
        let cls;
        if (st.streaming_error) {
          label = 'Error';
          cls = 'streaming-badge error';
        } else if (st.streaming_enabled) {
          label = 'Streaming';
          cls = 'streaming-badge active';
        } else {
          label = 'Idle';
          cls = 'streaming-badge idle';
        }
        // Append the live fps when the media service supplies it;
        // otherwise it'll be null and we just omit the suffix.
        if (st.streaming_fps != null) {
          label = `${label} (${st.streaming_fps} fps)`;
        }
        badge.textContent = label;
        badge.className = cls;
      }
      const urlEl = document.getElementById('streaming-url');
      if (urlEl && st && st.rtmp_url_masked) {
        urlEl.textContent = st.rtmp_url_masked || '-';
      }
      const cfg = await API.get('/config');
      if (cfg && cfg.video && cfg.video.streaming) {
        const el = document.getElementById('streaming-platform');
        if (el) el.textContent = cfg.video.streaming.platform || 'RTMP';
      }
      // Gate Start / Stop buttons on the operator's actual ability to
      // act. Start is disabled when streaming is admin-disabled, when
      // the URL or key is missing, or when a stream is already
      // running. Stop is disabled when nothing is running.
      this._gateButtons(cfg, st);
      // Speed status polling up to 1 Hz while a stream is live so fps
      // and error transitions are visible immediately; back to 3 s
      // when idle.
      this._setPollRate(
        st && st.streaming_enabled ? this._LIVE_POLL_MS : this._IDLE_POLL_MS,
      );
    } catch { /* swallow - status refresh is best-effort */ }
  },

  _gateButtons(cfg, status) {
    const startBtn = document.getElementById('streaming-start-btn');
    const stopBtn = document.getElementById('streaming-stop-btn');
    const hint = document.getElementById('streaming-button-hint');
    if (!startBtn || !stopBtn) return;

    const sc = (cfg && cfg.video && cfg.video.streaming) || {};
    const url = (sc.rtmp_url || '').trim();
    const key = (sc.stream_key || '').trim();
    const enabled = sc.enabled === true;
    const isStreaming = !!(status && status.streaming_enabled);

    let disableStart = false;
    let reason = '';
    if (!enabled) {
      disableStart = true;
      reason = 'Streaming is disabled in Config.';
    } else if (!url) {
      disableStart = true;
      reason = 'RTMP URL is empty in Config.';
    } else if (!key) {
      disableStart = true;
      reason = 'Stream Key is empty in Config.';
    } else if (isStreaming) {
      disableStart = true;
      // No hint here - the badge already says "Streaming".
    }

    startBtn.disabled = disableStart;
    stopBtn.disabled = !isStreaming;
    if (hint) hint.textContent = reason;
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
        // game_phase enum (apps/schemas/game_state.py::GamePhase):
        //   0 NORMAL, 1 PENALTY_SHOOTOUT, 2 OVERTIME, 3 TIMEOUT.
        // Dummy uses NORMAL so the row-1 phase indicator reads "1st".
        game_phase: 0,
        packet_number: 0,
        // Per-team message-budget values.
        team1_message_budget: 0,
        team2_message_budget: 0,
        // Defaults so the draw branches render their "nothing to show"
        // path without complaining.
        stopped: false,
        secondary_time: 0,
        // GK colour / number; absent (0) by default.
        team1_field_player_colour: null,
        team1_goalkeeper_colour: null,
        team1_goalkeeper: 0,
        team2_field_player_colour: null,
        team2_goalkeeper_colour: null,
        team2_goalkeeper: 0,
        // Shoot-out shot history.
        team1_penalty_shot: 0,
        team1_single_shots: 0,
        team2_penalty_shot: 0,
        team2_single_shots: 0,
      };
    }

    // 3b. Fetch the current per-robot penalty snapshot.
    // Best-effort - when no data is available the canvas just
    // doesn't draw the penalty card stacks. Penalty cards mirror
    // what cairooverlay's draw_penalty_cards draws on the RTMP
    // output.
    let penalties = null;
    try {
      const resp = await fetch('/api/v1/penalties/current');
      if (resp.ok && resp.status !== 204) {
        penalties = await resp.json();
      }
    } catch { /* no penalty data - overlay just skips the stacks */ }

    // 4. Draw the frame onto the canvas.
    const img = new Image();
    const url = URL.createObjectURL(frameBlob);
    img.onload = () => {
      canvas.width = img.naturalWidth || 960;
      canvas.height = img.naturalHeight || 540;
      ctx.drawImage(img, 0, 0);
      URL.revokeObjectURL(url);

      // 5. Draw the broadcast overlay.
      this._drawOverlay(ctx, canvas.width, canvas.height, gc, gcLive, penalties);

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

  _drawOverlay(ctx, w, h, gc, live, penalties) {
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
    // Per-team jersey colours (with the cairooverlay's red/blue
    // fallbacks when no colour is configured). When no teams.json
    // override exists, fall back to the
    // packet-driven field-player colour byte so the score box still
    // reflects the live GC packet (mirrors the Rust
    // `team_colour_byte_to_rgb` fallback path).
    const team1ColorFromByte = this._teamColourByteToCSS(gc.team1_field_player_colour);
    const team2ColorFromByte = this._teamColourByteToCSS(gc.team2_field_player_colour);
    const team1Color = this._resolveTeamColor(gc.team1_number, team1ColorFromByte || 'rgb(217, 25, 25)');
    const team2Color = this._resolveTeamColor(gc.team2_number, team2ColorFromByte || 'rgb(25, 50, 217)');
    // GK colour strip - only when the GK byte differs from the
    // field-player byte. Null suppresses the strip.
    const team1GKColor = this._resolveGoalkeeperColor(
      gc.team1_goalkeeper_colour,
      gc.team1_field_player_colour,
    );
    const team2GKColor = this._resolveGoalkeeperColor(
      gc.team2_goalkeeper_colour,
      gc.team2_field_player_colour,
    );

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
    this._drawScoreboard(
      ctx, w, h, gc, live, team1, team2, team1Color, team2Color,
      team1GKColor, team2GKColor,
      scale, margin, fontSm, fontMd, fontLg
    );

    // ---- Bottom-left / bottom-right: penalty cards ----
    if (penalties) {
      // Filter no-penalty entries and sort by secs_remaining
      // desc with player_number tie-break so the longest-pending
      // penalty sits at the top of the bottom-anchored stack and ties
      // don't flicker between frames. Mirrors `parse_penalties`.
      const prep = (arr) => (arr || [])
        .filter(p => p.penalty_code !== 0)
        .sort((a, b) =>
          (b.secs_remaining ?? 0) - (a.secs_remaining ?? 0)
          || (a.player_number ?? 0) - (b.player_number ?? 0));
      const t1Cards = prep(penalties.team1_penalties);
      const t2Cards = prep(penalties.team2_penalties);
      this._drawPenaltyCards(
        ctx, h, margin, scale, fontSm, fontMd, 'left',
        t1Cards, team1Color, margin, gc.team1_goalkeeper || 0);
      this._drawPenaltyCards(
        ctx, h, margin, scale, fontSm, fontMd, 'right',
        t2Cards, team2Color, w - margin, gc.team2_goalkeeper || 0);
    }
  },

  _drawScoreboard(ctx, w, h, gc, live, team1, team2, team1Color, team2Color,
                  team1GKColor, team2GKColor,
                  scale, margin, fontSm, fontMd, fontLg) {
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

    // Row-2 outer team-colour blocks + GK colour strips
    // (mirror of overlay.rs draw_scoreboard). Outer block ~10 px
    // scaled; GK strip ~5 px scaled and suppressed when equal to
    // the field-player colour.
    const outerColorW = 10 * scale;
    const gkStripW = 5 * scale;
    const team1GKSegment = team1GKColor ? (gkStripW + 2 * scale) : 0;
    const team2GKSegment = team2GKColor ? (gkStripW + 2 * scale) : 0;
    const totalW = outerColorW + gap + team1GKSegment + nameW + scoreW + gap
                 + clockW + gap + scoreW + nameW + team2GKSegment + gap + outerColorW;
    const sbX = (w - totalW) / 2;
    const sbH = rowH * 3 + pad * 2;
    const sbY = h - margin - sbH;

    // Background
    ctx.fillStyle = 'rgba(0, 0, 0, 0.65)';
    ctx.beginPath();
    ctx.roundRect(sbX - pad, sbY - pad, totalW + pad * 2, sbH + pad, r);
    ctx.fill();

    // Pre-compute the row-2 name-column anchors so row-1 packet
    // counts / shoot-out dots line up under the team-name columns
    // even after the GK strips widened the row.
    const xName1Left = sbX + outerColorW + gap + team1GKSegment;
    const xName2Right = xName1Left + nameW + scoreW + gap + clockW + gap + scoreW + nameW;
    const name1Center = xName1Left + nameW / 2;
    const name2Center = xName2Right - nameW / 2;

    // --- Row 1: shoot-out dots OR message-budget cells + phase ---
    // During PENALTY_SHOOTOUT the left/right cells render shoot-out
    // shot dots instead of the message-budget label. Outside
    // shoot-out, the cells read "msg <budget>".
    ctx.font = `bold ${fontSm}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.fillStyle = live ? '#cccccc' : '#ffcc66';
    const phase = this._formatGamePhase(gc);
    const isShootout = gc.game_phase === 1;
    const t1Shots = gc.team1_penalty_shot || 0;
    const t2Shots = gc.team2_penalty_shot || 0;
    const totalDots = isShootout ? Math.min(Math.max(t1Shots, t2Shots), 10) : 0;
    const yBase = sbY + fontSm;
    const dotCenterY = sbY + fontSm * 0.5;

    if (totalDots > 0) {
      this._drawShootoutDots(
        ctx, name1Center, dotCenterY, scale,
        this._shootoutDotStates(t1Shots, gc.team1_single_shots || 0, totalDots),
        team1Color,
      );
    } else {
      // Fallback chain mirrors overlay.rs::parse_game_state:
      //   team{1,2}_message_budget -> team{1,2}_packet_number (historical) -> packet_number (last resort).
      const legacyPkt = gc.packet_number || 0;
      const mb1 = gc.team1_message_budget ?? gc.team1_packet_number ?? legacyPkt;
      ctx.textAlign = 'center';
      ctx.fillText(`msg ${mb1}`, name1Center, yBase);
    }

    ctx.textAlign = 'center';
    ctx.fillStyle = live ? '#cccccc' : '#ffcc66';
    ctx.fillText(phase, w / 2, yBase);

    if (totalDots > 0) {
      this._drawShootoutDots(
        ctx, name2Center, dotCenterY, scale,
        this._shootoutDotStates(t2Shots, gc.team2_single_shots || 0, totalDots),
        team2Color,
      );
    } else {
      const legacyPkt = gc.packet_number || 0;
      const mb2 = gc.team2_message_budget ?? gc.team2_packet_number ?? legacyPkt;
      ctx.textAlign = 'center';
      ctx.fillStyle = live ? '#cccccc' : '#ffcc66';
      ctx.fillText(`msg ${mb2}`, name2Center, yBase);
    }

    // --- Row 2: outer color | gk? | name | score | clock | score | name | gk? | outer color ---
    const rowY = sbY + rowH;
    const yText = rowY + fontLg * 0.85;

    // Left outer team-colour block.
    ctx.fillStyle = team1Color;
    ctx.beginPath();
    ctx.roundRect(sbX, rowY, outerColorW, rowH, r);
    ctx.fill();

    // Left GK strip (only when distinct from field colour).
    if (team1GKColor) {
      ctx.fillStyle = team1GKColor;
      ctx.beginPath();
      ctx.roundRect(sbX + outerColorW + gap, rowY, gkStripW, rowH, r);
      ctx.fill();
    }

    // Team 1 name (right-aligned before score)
    ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'right';
    ctx.fillStyle = '#ffffff';
    const xName1Right = xName1Left + nameW;
    ctx.fillText(team1, xName1Right - pad, yText);

    // Score 1 - team 1 jersey colour (or red fallback).
    // Digit colour adapts to the team-colour background's
    // luminance so bright jerseys (white / yellow / cyan) don't
    // render an invisible white-on-white digit.
    const xScore1 = xName1Right;
    ctx.fillStyle = team1Color;
    ctx.beginPath();
    ctx.roundRect(xScore1, rowY, scoreW, rowH, r);
    ctx.fill();
    ctx.font = `bold ${fontLg}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'center';
    ctx.fillStyle = this._contrastingTextColor(team1Color);
    ctx.fillText(String(gc.team1_score), xScore1 + scoreW / 2, yText);

    // Clock (dark box). No pause-glyph prefix when stopped - most
    // system fonts on the streaming box don't have a glyph for it and
    // render an empty box, which operators read as a malformed minus
    // sign. The 60%-alpha dim already conveys "paused". Also: show
    // ASCII `-` when the
    // clock has gone negative (overtime / past final whistle)
    // instead of silently rendering the overtime delta as a
    // positive number.
    const xClock = xScore1 + scoreW + gap;
    ctx.fillStyle = 'rgba(38, 38, 38, 0.9)';
    ctx.beginPath();
    ctx.roundRect(xClock, rowY, clockW, rowH, r);
    ctx.fill();
    const negative = (gc.secs_remaining ?? 0) < 0;
    const mins = Math.floor(Math.abs(gc.secs_remaining ?? 0) / 60);
    const secs = Math.abs(gc.secs_remaining ?? 0) % 60;
    const sign = negative ? '-' : '';
    const clockStr = `${sign}${String(mins).padStart(2, '0')}:${String(secs).padStart(2, '0')}`;
    ctx.fillStyle = gc.stopped ? 'rgba(255, 255, 255, 0.6)' : '#ffffff';
    ctx.fillText(clockStr, xClock + clockW / 2, yText);

    // Score 2 - team 2 jersey colour (or blue fallback).
    // Same contrast-adaptive treatment as score 1.
    const xScore2 = xClock + clockW + gap;
    ctx.fillStyle = team2Color;
    ctx.beginPath();
    ctx.roundRect(xScore2, rowY, scoreW, rowH, r);
    ctx.fill();
    ctx.fillStyle = this._contrastingTextColor(team2Color);
    ctx.fillText(String(gc.team2_score), xScore2 + scoreW / 2, yText);

    // Team 2 name (left-aligned after score)
    ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'left';
    ctx.fillStyle = '#ffffff';
    const xName2Left = xScore2 + scoreW;
    ctx.fillText(team2, xName2Left + pad, yText);

    // Right GK strip (mirror of left).
    if (team2GKColor) {
      ctx.fillStyle = team2GKColor;
      ctx.beginPath();
      ctx.roundRect(xName2Left + nameW, rowY, gkStripW, rowH, r);
      ctx.fill();
    }

    // Right outer team-colour block (mirror of left).
    ctx.fillStyle = team2Color;
    ctx.beginPath();
    ctx.roundRect(sbX + totalW - outerColorW, rowY, outerColorW, rowH, r);
    ctx.fill();

    // --- Row 3: game state ---
    const stateY = sbY + rowH * 2;
    ctx.font = `bold ${fontSm}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'center';
    ctx.fillStyle = live ? '#e0e0e0' : '#ffcc66';
    const stateLine = this._formatStateLine(gc, team1, team2);
    ctx.fillText(stateLine, w / 2, stateY + fontSm + 2 * scale);
  },

  /**
   * Mirrors `shootout_dot_states` in overlay.rs: classify
   * each dot as filled (scored), hollow (missed), or dim (upcoming).
   * Returns an array of state strings ('scored' | 'missed' | 'upcoming').
   */
  _shootoutDotStates(penaltyShot, singleShots, total) {
    const out = [];
    for (let i = 0; i < total; i++) {
      const taken = i < penaltyShot;
      const scored = ((singleShots >> i) & 1) !== 0;
      if (taken && scored) out.push('scored');
      else if (taken) out.push('missed');
      else out.push('upcoming');
    }
    return out;
  },

  /**
   * Mirrors `draw_shootout_dots` in overlay.rs: render one
   * team's shoot-out dot row centred at (centerX, centerY).
   *   scored   = filled team-colour dot
   *   missed   = team-colour outline
   *   upcoming = dim grey outline (~30% alpha)
   */
  _drawShootoutDots(ctx, centerX, centerY, scale, dots, teamColor) {
    if (!dots.length) return;
    const dotR = 3 * scale;
    const gap = 3 * scale;
    const rowW = dots.length * (dotR * 2) + (dots.length - 1) * gap;
    let x = centerX - rowW / 2 + dotR;
    for (const state of dots) {
      ctx.beginPath();
      ctx.arc(x, centerY, dotR, 0, Math.PI * 2);
      if (state === 'scored') {
        ctx.fillStyle = teamColor;
        ctx.fill();
      } else if (state === 'missed') {
        ctx.strokeStyle = teamColor;
        ctx.lineWidth = 1.5 * scale;
        ctx.stroke();
      } else {
        ctx.strokeStyle = 'rgba(178, 178, 178, 0.3)';
        ctx.lineWidth = 1 * scale;
        ctx.stroke();
      }
      x += dotR * 2 + gap;
    }
  },

  /**
   * Mirrors `format_game_phase` in overlay.rs: map the GamePhase enum
   * byte + first_half to the row-1 centre label.
   *   NORMAL (0)           -> "1st" / "2nd"
   *   PENALTY_SHOOTOUT (1) -> "shootout"
   *   OVERTIME (2)         -> "ET 1st" / "ET 2nd"
   *   TIMEOUT (3)          -> falls through to NORMAL behaviour
   *                          (the state row already shows "timeout")
   */
  _formatGamePhase(gc) {
    const halfLabel = gc.first_half ? '1st' : '2nd';
    switch (gc.game_phase) {
      case 1: return 'shootout';
      case 2: return gc.first_half ? 'ET 1st' : 'ET 2nd';
      default: return halfLabel; // NORMAL, TIMEOUT, unknown
    }
  },

  /**
   * Draw a stack of penalty cards anchored to the bottom-left or
   * bottom-right of the frame. Mirrors the Rust draw_penalty_cards
   * in apps/media_service/src/overlay.rs.
   *
   * Each card has three rows:
   *   1. Player number on team jersey colour (top)
   *   2. Countdown MM:SS on light grey (middle)
   *   3. Penalty reason text (bottom)
   *
   * Cards stack from the bottom margin upward; oldest card ends up
   * highest. Empty list draws nothing.
   */
  _drawPenaltyCards(ctx, h, margin, scale, fontSm, fontMd, side, cards, color, xAnchor, goalkeeperNumber) {
    if (!cards || cards.length === 0) return;

    const cardW = 80 * scale;
    const rowH = 18 * scale;
    const cardH = rowH * 3;
    const gap = 4 * scale;
    const r = 3 * scale;

    const totalH = (cardH + gap) * cards.length - gap;
    let y = h - margin - totalH;

    for (const card of cards) {
      const x = side === 'left' ? xAnchor : xAnchor - cardW;
      // GK badge on the penalised player's card when that
      // player is the team's goalkeeper. Mirrors the Rust
      // `is_goalkeeper` flag wiring.
      const isGoalkeeper = goalkeeperNumber > 0
        && (card.player_number ?? 0) === goalkeeperNumber;
      this._drawOnePenaltyCard(
        ctx, x, y, cardW, rowH, r, fontSm, fontMd, card, color,
        isGoalkeeper,
      );
      y += cardH + gap;
    }
  },

  _drawOnePenaltyCard(ctx, x, y, w, rowH, r, fontSm, fontMd, card, color, isGoalkeeper) {
    const cardH = rowH * 3;

    // Outer dark backing (the third row's background, since rows 1
    // and 2 are painted over the top two thirds).
    ctx.fillStyle = 'rgba(13, 13, 13, 0.85)';
    ctx.beginPath();
    ctx.roundRect(x, y, w, cardH, r);
    ctx.fill();

    // Row 1 - player number on team colour. Digit colour adapts to
    // the team-colour background's luminance so
    // bright jerseys (white / yellow / cyan) don't render an
    // invisible white-on-white digit.
    ctx.fillStyle = color;
    ctx.beginPath();
    ctx.roundRect(x, y, w, rowH, r);
    ctx.fill();
    ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.textAlign = 'center';
    ctx.fillStyle = this._contrastingTextColor(color);
    ctx.fillText(String(card.player_number ?? 0), x + w / 2, y + rowH * 0.75);

    // Small "GK" badge top-left of row 1 when this player is
    // the team's goalkeeper. Sits inside the row so it doesn't push
    // the player-number off-centre.
    if (isGoalkeeper) {
      const badgeFont = fontSm * 0.85;
      ctx.font = `bold ${badgeFont}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
      ctx.textAlign = 'left';
      const badgeText = 'GK';
      const badgeM = ctx.measureText(badgeText);
      const padX = rowH * 0.18;
      const padY = rowH * 0.12;
      const badgeW = badgeM.width + padX * 2;
      const badgeH = badgeFont + padY * 2;
      const bx = x + rowH * 0.15;
      const by = y + rowH * 0.15;
      ctx.fillStyle = 'rgba(13, 13, 13, 0.85)';
      ctx.beginPath();
      ctx.roundRect(bx, by, badgeW, badgeH, r * 0.6);
      ctx.fill();
      ctx.fillStyle = '#ffffff';
      ctx.fillText(badgeText, bx + padX, by + padY + badgeFont * 0.85);
      // Restore row-1 font + alignment for any later draws.
      ctx.font = `bold ${fontMd}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
      ctx.textAlign = 'center';
    }

    // Row 2 - timer MM:SS on light grey.
    const row2Y = y + rowH;
    ctx.fillStyle = '#d9d9d9';
    ctx.beginPath();
    ctx.roundRect(x, row2Y, w, rowH, r * 0.3);
    ctx.fill();
    const secs = card.secs_remaining ?? 0;
    const mins = Math.floor(secs / 60);
    const s = secs % 60;
    const t = `${String(mins).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
    ctx.fillStyle = '#0d0d0d';
    ctx.fillText(t, x + w / 2, row2Y + rowH * 0.75);

    // Row 3 - penalty reason text.
    const row3Y = y + 2 * rowH;
    ctx.font = `bold ${fontSm}px "Noto Sans", "Helvetica Neue", Helvetica, Arial, sans-serif`;
    ctx.fillStyle = '#ffffff';
    ctx.fillText(card.penalty_reason || '', x + w / 2, row3Y + rowH * 0.7);
  },

  _formatStateLine(gc, team1, team2) {
    const state = (gc.state || 'PLAYING').toLowerCase();
    const setPlay = gc.set_play || 'NONE';
    // When the GC reports no kicker (raw 255 normalised to 0
    // upstream, or genuinely 0 pre-kickoff), drop the "for <kicker>"
    // suffix entirely rather than rendering "kickoff for unknown".
    const resolveKicker = () => {
      if (!gc.kicking_team) return null;
      if (gc.kicking_team === gc.team1_number) return team1;
      if (gc.kicking_team === gc.team2_number) return team2;
      return 'unknown';
    };
    let head;
    if (setPlay !== 'NONE') {
      const play = setPlay.toLowerCase().replace(/_/g, ' ');
      const kicker = resolveKicker();
      head = kicker ? `${state}, ${play} for ${kicker}` : `${state}, ${play}`;
    } else if (gc.state === 'READY' || gc.state === 'SET') {
      const kicker = resolveKicker();
      head = kicker ? `${state}, kickoff for ${kicker}` : state;
    } else {
      head = state;
    }
    // Append the GC secondary clock (ready countdown, free-kick wait,
    // half-time break) when it's running, e.g. "ready, kickoff for
    // HTWK Robots - 00:21".
    const secondary = gc.secondary_time ?? 0;
    if (secondary > 0) {
      const m = Math.floor(secondary / 60);
      const s = secondary % 60;
      return `${head} - ${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
    }
    return head;
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
