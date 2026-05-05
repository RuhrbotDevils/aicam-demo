// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * Dashboard page - health, system metrics, camera_preview.
 *
 * System metrics + service statuses are polled at 1 Hz from
 * /api/v1/system/metrics. Service statuses come from systemd via
 * the same endpoint.
 *
 * camera_preview is the raw live camera feed. Do not conflate with
 * object_detection_preview (detection.js) or field_preview (field_detection.js).
 */
const DashboardPage = {
  _metricsInterval: null,
  _liveInterval: null,
  _cameraPreviewStarted: false,
  _restartCooldowns: {},  // service_name → timestamp when cooldown expires
  _replayState: { active: false, position_s: 0, duration_s: 0 },
  _lastReplayActive: false,
  _lastReplayPos: -1,

  async render(container) {
    container.innerHTML = `
      <h1>Dashboard <span id="dash-node-id" style="font-weight:normal;font-size:0.7em;color:var(--text-muted)"></span></h1>
      <div class="card">
        <h2>Camera Preview</h2>
        <div class="det-controls">
          <button class="btn btn-primary" id="dash-snap-btn" onclick="DashboardPage.snap()">Snap</button>
          <label class="det-live-toggle">
            <input type="checkbox" id="dash-live-toggle" onchange="DashboardPage.toggleLive(this.checked)">
            <span>Live (~1 fps)</span>
          </label>
        </div>
        <div class="preview-container" id="camera-preview-box">
          <span class="preview-placeholder">Click Snap or enable Live to see the camera preview</span>
        </div>
      </div>

      <div id="replay-control-card" style="display:none"></div>

      <div class="dash-metrics-row">
        <div class="card" id="services-card">
          <h2>Services</h2>
          <div class="loading">Loading...</div>
        </div>
        <div class="card" id="metrics-card">
          <h2>System Metrics</h2>
          <div class="loading">Loading...</div>
        </div>
      </div>
    `;
    this._lastReplayActive = false;
    this._lastReplayPos = -1;
    this._replayState = { active: false, position_s: 0, duration_s: 0 };
    Magnifier.attach(document.getElementById('camera-preview-box'));
    this._refreshNodeId();
    this._refreshMetrics();
    this._refreshReplay();
    this._metricsInterval = setInterval(() => { this._refreshMetrics(); this._refreshReplay(); }, 1000);
  },

  destroy() {
    clearInterval(this._metricsInterval);
    this._metricsInterval = null;
    this._stopLive();
  },

  // ------------------------------------------------------------------
  // Replay state (1 Hz, piggybacked on metrics interval)
  // ------------------------------------------------------------------

  _formatHHMMSS(seconds) {
    const s = Math.max(0, Math.floor(seconds));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    return `${h.toString().padStart(2, '0')}:${m.toString().padStart(2, '0')}:${sec.toString().padStart(2, '0')}`;
  },

  async _refreshReplay() {
    try {
      const res = await fetch(`${API.base}/replay/status`);
      if (!res.ok) return;
      const data = await res.json();
      const active = !!data.active;
      const position_s = data.position_s || 0;
      const duration_s = data.duration_s || 0;

      this._replayState = { active, position_s, duration_s };

      const posChanged = Math.floor(position_s) !== this._lastReplayPos;
      const activeChanged = active !== this._lastReplayActive;

      if (activeChanged || (active && posChanged)) {
        this._lastReplayActive = active;
        this._lastReplayPos = Math.floor(position_s);
        this._renderReplayPanel();
        // Notify app.js to update nav visibility
        if (typeof Router !== 'undefined' && Router._updateReplayNav) {
          Router._updateReplayNav(active);
        }
      }
    } catch { /* best-effort - media service may not be running */ }
  },

  _renderReplayPanel() {
    const card = document.getElementById('replay-control-card');
    if (!card) return;
    const { active, position_s, duration_s } = this._replayState;

    if (active) {
      card.style.display = '';
      card.className = 'card';
      card.innerHTML = `
        <h2>Replay</h2>
        <div class="rec-active">
          <span class="rec-dot" style="background:var(--accent)"></span>
          <strong>Replay active:</strong>
          <span class="rec-elapsed">${this._formatHHMMSS(position_s)} (of ${this._formatHHMMSS(duration_s)})</span>
        </div>
        <div style="margin-top: 0.8rem">
          <button class="btn btn-danger" onclick="DashboardPage.stopReplay()">Stop replay</button>
        </div>
      `;
    } else {
      card.style.display = 'none';
      card.innerHTML = '';
    }
  },

  async stopReplay() {
    try {
      const res = await fetch(`${API.base}/replay/stop`, { method: 'POST' });
      if (res.ok) {
        Notify.success('Replay stopped');
        this._replayState = { active: false, position_s: 0, duration_s: 0 };
        this._lastReplayActive = false;
        this._lastReplayPos = -1;
        this._renderReplayPanel();
        if (typeof Router !== 'undefined' && Router._updateReplayNav) {
          Router._updateReplayNav(false);
        }
      } else {
        Notify.error('Failed to stop replay');
      }
    } catch {
      Notify.error('Failed to stop replay');
    }
  },

  // ------------------------------------------------------------------
  // Node ID (one-time fetch)
  // ------------------------------------------------------------------

  async _refreshNodeId() {
    try {
      const cfg = await API.get('/config');
      const el = document.getElementById('dash-node-id');
      if (el && cfg.node && cfg.node.id) {
        el.textContent = '(' + cfg.node.id + ')';
      }
    } catch { /* best-effort */ }
  },

  // ------------------------------------------------------------------
  // System metrics + services (1 Hz)
  // ------------------------------------------------------------------

  async _refreshMetrics() {
    try {
      const m = await API.get('/system/metrics');
      this._renderServices(m.services || []);
      this._renderMetrics(m);
    } catch { /* best-effort */ }
  },

  async _renderServices(services) {
    const el = document.getElementById('services-card');
    if (!el) return;

    // Fetch service health data
    let healthMap = {};
    try {
      const h = await API.get('/services/health');
      healthMap = h.services || {};
    } catch { /* health data optional */ }

    const rows = services.map(s => {
      const badge = this._badge(s.status);
      const inCooldown = this._isInCooldown(s.name);
      const disabled = inCooldown ? ' disabled' : '';
      const label = inCooldown ? 'Wait...' : 'Restart';
      const isFailed = s.status === 'failed' || s.status === 'stopped';
      const btnClass = isFailed ? 'btn btn-sm btn-warning' : 'btn btn-sm btn-secondary';
      const btnStyle = isFailed ? '' : ' style="font-size:0.75rem"';
      const actionHtml = `<button class="${btnClass}"${btnStyle}${disabled}
        onclick="DashboardPage.restartService('${s.name}')">${label}</button>`;

      // Health badge: match by short name (e.g. "tracker" in "ai-cam-tracker")
      const shortName = s.name.replace('ai-cam-', '');
      const health = healthMap[shortName];
      let healthBadge = '';
      if (health) {
        const drops = health.drops_last_10s || 0;
        const cls = drops > 0 ? 'badge-warn' : 'badge-ok';
        const title = `processed: ${health.frames_processed}, dropped: ${health.frames_dropped}, drops/10s: ${drops}`;
        healthBadge = `<span class="badge ${cls}" title="${title}">${drops > 0 ? drops + ' drops' : 'ok'}</span>`;
      }

      return `<tr><td>${s.name}</td><td>${badge}</td><td>${healthBadge}</td><td>${actionHtml}</td></tr>`;
    }).join('');

    el.innerHTML = `
      <h2>Services</h2>
      <table><thead><tr><th>Service</th><th>Status</th><th>Health</th><th>Action</th></tr></thead>
      <tbody>${rows}</tbody></table>
    `;
  },

  async restartService(name) {
    if (this._isInCooldown(name)) return;
    this._restartCooldowns[name] = Date.now() + 5000;
    try {
      await API.post(`/system/restart/${name}`);
      Notify.success(`Restarting ${name}...`);
    } catch (e) {
      Notify.error(`Failed to restart ${name}: ${e.message}`);
    }
  },

  _isInCooldown(name) {
    const until = this._restartCooldowns[name];
    if (!until) return false;
    if (Date.now() >= until) {
      delete this._restartCooldowns[name];
      return false;
    }
    return true;
  },

  _renderMetrics(m) {
    const el = document.getElementById('metrics-card');
    if (!el) return;

    // CPU bars
    const cpuTotal = this._progressBar('CPU Total', m.cpu.total, '%');
    const cpuCores = (m.cpu.cores || []).map((v, i) =>
      this._progressBar(`Core ${i}`, v, '%', true)
    ).join('');

    // Preserve <details> open state across refreshes
    const coresOpen = document.querySelector('.cpu-cores-details')?.open || false;

    // Temperatures - display as one decimal place (e.g. 41.5 C).
    const fmtTemp = (v) => v != null ? `${v.toFixed(1)} C` : 'n/a';
    const socTemp = fmtTemp(m.temperature.soc);
    const socClass = m.temperature.soc > 75 ? 'metric-warn' : m.temperature.soc > 65 ? 'metric-caution' : '';
    // Hailo: prefer per-thermistor (TS0/TS1) when available.
    // Hailo-10H has two on-chip thermistors and we surface
    // both.
    const hailoTs0 = m.temperature.hailo_ts0;
    const hailoTs1 = m.temperature.hailo_ts1;
    const hailoMax = m.temperature.hailo;
    const hailoTempStr = fmtTemp(hailoMax);
    const hailoTempDetail = (hailoTs0 != null && hailoTs1 != null)
      ? `<div class="stats-muted" style="font-size:0.75em;text-align:right;margin-top:-0.15rem">(TS0 ${hailoTs0.toFixed(1)} / TS1 ${hailoTs1.toFixed(1)})</div>`
      : '';
    const hailoClass = hailoMax > 85 ? 'metric-warn' : hailoMax > 70 ? 'metric-caution' : '';

    // RTC battery (Pi 5 PMIC BATT_V channel) - display voltage with
    // three decimal places (e.g. 3.309 V) consistent across states.
    const rtc = m.rtc_battery || {};
    const rtcV = rtc.voltage_v;
    const rtcState = rtc.state || 'unknown';
    const rtcVStr = rtcV != null ? `${rtcV.toFixed(3)} V` : 'n/a';
    let rtcText, rtcClass;
    if (rtcState === 'missing') {
      rtcText = `${rtcVStr} - no battery`;
      rtcClass = 'metric-warn';
    } else if (rtcState === 'low') {
      rtcText = `${rtcVStr} - low`;
      rtcClass = 'metric-warn';
    } else if (rtcState === 'ok') {
      rtcText = rtcVStr;
      rtcClass = '';
    } else {
      rtcText = 'n/a';
      rtcClass = 'stats-muted';
    }

    // Memory
    const memPct = m.memory.percent || 0;
    const memText = `${m.memory.used_mb} / ${m.memory.total_mb} MB`;

    // Swap
    const swapPct = m.memory.swap_percent || 0;
    const swapTotal = m.memory.swap_total_mb || 0;
    const swapUsed = m.memory.swap_used_mb || 0;

    // Disk
    const diskPct = m.disk.used_percent || 0;
    const diskFree = m.disk.free_gb || 0;

    // Streaming FPS
    const fpsHtml = m.streaming_fps != null
      ? `<div class="metric-row"><span class="metric-label">Streaming FPS</span><span class="metric-value">${m.streaming_fps}</span></div>`
      : '';

    el.innerHTML = `
      <h2>System Metrics</h2>
      <div class="metrics-grid">
        <div class="metrics-section">
          <h3>CPU</h3>
          ${cpuTotal}
          <details class="cpu-cores-details"${coresOpen ? ' open' : ''}><summary>Per-core</summary>${cpuCores}</details>
        </div>
        <div class="metrics-section">
          <h3>Temperature</h3>
          <div class="metric-row"><span class="metric-label">SoC</span><span class="metric-value ${socClass}">${socTemp}</span></div>
          <div class="metric-row"><span class="metric-label">Hailo</span><span class="metric-value ${hailoClass}">${hailoTempStr}</span></div>
          ${hailoTempDetail}
        </div>
        <div class="metrics-section">
          <h3>RTC Battery</h3>
          <div class="metric-row"><span class="metric-label">Voltage</span><span class="metric-value ${rtcClass}">${rtcText}</span></div>
        </div>
        <div class="metrics-section">
          <h3>Memory</h3>
          ${this._progressBar(memText, memPct, '%')}
          ${swapTotal > 0 ? this._progressBar(`Swap: ${swapUsed} / ${swapTotal} MB`, swapPct, '%', true) : ''}
        </div>
        <div class="metrics-section">
          <h3>Disk</h3>
          ${this._progressBar(`${diskFree} GB free`, diskPct, '%')}
        </div>
        ${fpsHtml ? `<div class="metrics-section"><h3>Streaming</h3>${fpsHtml}</div>` : ''}
      </div>
    `;
  },

  _progressBar(label, value, unit, small) {
    const pct = Math.min(100, Math.max(0, value || 0));
    const cls = pct > 90 ? 'bar-danger' : pct > 75 ? 'bar-warn' : 'bar-ok';
    const height = small ? 'progress-sm' : '';
    return `<div class="metric-bar-row ${height}">
      <span class="metric-label">${label}</span>
      <div class="metric-bar">
        <div class="metric-bar-fill ${cls}" style="width:${pct}%"></div>
      </div>
      <span class="metric-pct">${Math.round(pct)}${unit}</span>
    </div>`;
  },

  // ------------------------------------------------------------------
  // Camera preview
  // ------------------------------------------------------------------

  async snap() {
    const btn = document.getElementById('dash-snap-btn');
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
    const toggle = document.getElementById('dash-live-toggle');
    if (toggle) toggle.checked = false;
  },

  async _fetchFrame() {
    const box = document.getElementById('camera-preview-box');
    if (!box) return;
    try {
      const resp = await fetch(`/api/v1/camera_preview/frame?t=${Date.now()}`);
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

  _badge(status) {
    const cls = { running: 'badge-ok', idle: 'badge-muted', stub: 'badge-muted',
                  enabled: 'badge-ok', disabled: 'badge-muted',
                  error: 'badge-err', failed: 'badge-err', degraded: 'badge-warn',
                  stopped: 'badge-muted', starting: 'badge-warn',
                  'not-found': 'badge-muted',
                  active: 'badge-ok', connected: 'badge-ok',
                  calibrated: 'badge-ok', uncalibrated: 'badge-warn',
                  joined_uncalibrated: 'badge-warn', joining: 'badge-warn',
                  disconnected: 'badge-err' }[status] || 'badge-muted';
    return `<span class="badge ${cls}">${status}</span>`;
  },
};
