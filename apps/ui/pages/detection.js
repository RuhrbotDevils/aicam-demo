// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * Detection page - AI model status + object_detection_preview (snap/live).
 *
 * object_detection_preview is the Hailo-annotated frame stream fed from the
 * ai_sink appsink in the media service. Do not conflate with camera_preview
 * (dashboard/recording raw feed) or field_preview (calibration page).
 */
const DetectionPage = {
  _liveInterval: null,
  _statusInterval: null,
  // Cached preview backend so toggleLive / snap know which path to take
  // without a fresh /config GET each click. Refreshed on every status
  // tick (5 s) and right after setPreviewBackend.
  _previewBackend: 'hailo',

  async render(container) {
    container.innerHTML = `
      <h1>Object Detection</h1>
      <div class="card" id="det-status-card">
        <h2>Detection Status</h2>
        <div id="det-status" class="loading">Loading...</div>
      </div>
      <div class="card" id="object-detection-preview-card">
        <h2>Object Detection Preview</h2>
        <div class="det-controls">
          <button class="btn btn-primary" id="det-snap-btn" onclick="DetectionPage.snap()">Snap</button>
          <label class="det-live-toggle" id="det-live-toggle-label">
            <input type="checkbox" id="det-live-toggle" onchange="DetectionPage.toggleLive(this.checked)">
            <span>Live (~1 fps)</span>
          </label>
          <button class="btn btn-sm btn-secondary" id="det-snapshot-btn" onclick="DetectionPage.takeSnapshot()">Take Snapshot</button>
        </div>
        <div class="preview-container" id="object-detection-preview-box">
          <span class="preview-placeholder">Click Snap to see annotated frames</span>
        </div>
      </div>
    `;
    // Guard against the magnifier widget not being loaded and against
    // the tile being absent.
    if (typeof Magnifier !== 'undefined') {
      const tile = document.getElementById('object-detection-preview-box');
      if (tile) Magnifier.attach(tile);
    }
    this._refreshStatus();
    this._statusInterval = setInterval(() => this._refreshStatus(), 5000);
  },

  destroy() {
    this._stopLive();
    clearInterval(this._statusInterval);
    this._statusInterval = null;
  },

  async _refreshStatus() {
    try {
      const [data, cfg] = await Promise.all([
        API.get('/detection/status'),
        API.get('/config'),
      ]);
      const el = document.getElementById('det-status');
      if (!el) return;

      const hailo = data.hailo || { active: false, model: null };
      const cpu = data.cpu || { active: false, model: null };
      // Snap / Live preview uses Hailo when cpu_detection is false
      // (proxies the hailooverlay JPEG) and CPU when true (the Snap
      // button POSTs /detection/cpu_snap to run inference on demand).
      const previewBackend = cfg && cfg.features && cfg.features.cpu_detection ? 'cpu' : 'hailo';
      this._previewBackend = previewBackend;
      this._applyBackendVisibility();

      let html = '<div class="det-model-grid">';
      html += this._modelCard('Object Detection (Hailo)', hailo.model, hailo.active, 'hailo', previewBackend);
      html += this._modelCard('Object Detection (CPU)', cpu.model, cpu.active, 'cpu', previewBackend);
      html += '</div>';

      if (!hailo.active && !cpu.active) {
        html += `<p style="color: var(--text-muted); margin-top: 0.5rem">
          No detection backend is active. Select a model on the
          <a href="#config" style="color:var(--accent)">Configuration</a> page
          and ensure Hailo is available or the CPU detection feature flag is enabled.
        </p>`;
      }
      el.innerHTML = html;
    } catch { /* handled by API */ }
  },

  /**
   * Switch the Snap / Live preview between Hailo and CPU.
   *
   * Hailo mode: preview = pre-annotated JPEG from media service
   * (hailooverlay), Live (~1 fps) polling supported.
   *
   * CPU mode: preview = `/tmp/aicam-frames/cpu_annotated.jpg` written
   * on demand by `POST /api/v1/detection/cpu_snap`. Live polling does
   * not make sense (each Snap costs seconds of inference), so the
   * Live checkbox is hidden and any active Live polling is stopped.
   *
   * The cpu_detector systemd service is not auto-started - Snap on
   * the Detection page is the only way to trigger CPU inference, and
   * the control_api runs the inference inline.
   */
  async setPreviewBackend(backend) {
    try {
      const cfg = await API.get('/config');
      cfg.features.cpu_detection = (backend === 'cpu');
      await API.put('/config', cfg);
      this._previewBackend = backend;
      // Switching to CPU stops any active Live polling and unchecks
      // the box. Live makes no sense for an on-demand CPU snap.
      if (backend === 'cpu') {
        this._stopLive();
      }
      this._applyBackendVisibility();
      await this._refreshStatus();
      const box = document.getElementById('object-detection-preview-box');
      if (box && box.querySelector('img')) {
        this._fetchFrame();
      }
      Notify.success(`Preview backend: ${backend === 'cpu' ? 'CPU' : 'Hailo'}`);
    } catch {
      Notify.error('Failed to switch preview backend');
    }
  },

  /** Hide the Live (~1 fps) checkbox while CPU mode is selected. */
  _applyBackendVisibility() {
    const liveLabel = document.getElementById('det-live-toggle-label');
    if (!liveLabel) return;
    liveLabel.style.display = (this._previewBackend === 'cpu') ? 'none' : '';
  },

  /**
   * Render one model status card.
   *
   * `backend` is `'hailo'` or `'cpu'`; `previewBackend` is the
   * backend currently selected for Snap / Live (drives the radio
   * checked state). The radio writes features.cpu_detection via
   * `setPreviewBackend`.
   */
  _modelCard(title, model, isActive, backend, previewBackend) {
    const radio = backend && previewBackend !== undefined
      ? `<label class="det-preview-radio" style="display:flex;align-items:center;gap:0.4rem;font-size:0.8rem;margin-top:0.5rem">
          <input type="radio" name="det-preview-backend" value="${backend}"
            ${backend === previewBackend ? 'checked' : ''}
            onchange="DetectionPage.setPreviewBackend('${backend}')">
          <span>Use for Snap / Live</span>
        </label>`
      : '';

    if (!model) {
      return `<div class="det-model-card">
        <strong>${this._escape(title)}</strong>
        <span class="badge badge-muted">not loaded</span>
        ${radio}
      </div>`;
    }

    const badge = isActive
      ? '<span class="badge badge-ok">active</span>'
      : '<span class="badge badge-muted">loaded, idle</span>';

    const name = this._escape(model.display_name || '-');
    const dims = `${model.input_width}x${model.input_height}`;
    const fmt = this._escape(model.input_format || '');
    const outFmt = this._escape(model.output_format || '');
    const labels = model.labels ? this._escape(model.labels) : null;
    const notes = model.notes ? this._escape(model.notes) : null;

    return `<div class="det-model-card">
      <strong>${this._escape(title)}</strong>
      ${badge}
      <div class="det-model-info" style="font-weight:600;margin-top:0.25rem">${name}</div>
      <div class="det-model-meta" style="font-size:0.8rem;color:var(--text-muted);margin-top:0.25rem">
        <div>Input: <span style="color:var(--text)">${dims} ${fmt}</span></div>
        <div>Output format: <span style="color:var(--text)">${outFmt}</span></div>
        ${labels ? `<div>Labels: <span style="color:var(--text)">${labels}</span></div>` : ''}
      </div>
      ${notes ? `<div class="det-model-notes" style="font-size:0.8rem;color:var(--text-muted);margin-top:0.4rem;font-style:italic">${notes}</div>` : ''}
      ${radio}
    </div>`;
  },

  _escape(s) {
    if (s == null) return '';
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  },

  async snap() {
    const btn = document.getElementById('det-snap-btn');
    if (btn) { btn.disabled = true; btn.classList.add('btn-busy'); }
    try {
      // CPU mode: trigger one inference inline before refreshing the
      // preview. The endpoint loads YOLO on first call (~3-5 s) and
      // runs inference (~1-3 s) per call thereafter; we surface a
      // notification on configuration errors (no model selected) so
      // the user knows to configure a CPU model first.
      if (this._previewBackend === 'cpu') {
        try {
          await API.post('/detection/cpu_snap');
        } catch (e) {
          // The API helper already shows a Notify.error for non-2xx;
          // bail out so we don't render a stale frame.
          return;
        }
      }
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
    const toggle = document.getElementById('det-live-toggle');
    if (toggle) toggle.checked = false;
  },

  async takeSnapshot() {
    const btn = document.getElementById('det-snapshot-btn');
    if (btn) { btn.disabled = true; btn.classList.add('btn-busy'); }
    try {
      const result = await API.post('/detection/snapshot');
      if (result.ok) {
        Notify.success(`Snapshot saved (${result.detection_count} detections)`);
      } else {
        Notify.error(result.error || 'Snapshot failed');
      }
    } catch { Notify.error('Snapshot request failed'); }
    if (btn) { btn.disabled = false; btn.classList.remove('btn-busy'); }
  },

  async _fetchFrame() {
    const box = document.getElementById('object-detection-preview-box');
    if (!box) return;
    try {
      const resp = await fetch(`/api/v1/object_detection_preview/frame?t=${Date.now()}`);
      if (resp.status === 204) {
        box.innerHTML = '<span class="preview-placeholder">No annotated frame available</span>';
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
      box.innerHTML = '<span class="preview-placeholder">Failed to fetch annotated frame</span>';
    }
  },
};
