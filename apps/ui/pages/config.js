// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * Configuration page - view and edit system configuration.
 */
const ConfigPage = {
  _config: null,

  async render(container) {
    container.innerHTML = `
      <h1>Configuration</h1>
      <div id="config-container" class="loading">Loading...</div>
    `;
    await this._load();
  },

  destroy() {},

  async _load() {
    try {
      this._config = await API.get('/config');
      // Unified model registry. Models are identified by
      // display_name only - the UI never sees or sends filenames.
      try {
        const allOdModels = await API.get('/models?scope=object_detection');
        this._objectDetectionModels = allOdModels.filter(m => m.runtime !== 'pytorch');
        this._cpuObjectDetectionModels = allOdModels.filter(m => m.runtime === 'pytorch');
        this._selectedObjectDetectionModel = (await API.get('/models/selected?scope=object_detection')).display_name;
        this._selectedCpuObjectDetectionModel = (await API.get('/models/selected?scope=object_detection_cpu')).display_name;
      } catch {
        this._objectDetectionModels = [];
        this._cpuObjectDetectionModels = [];
        this._selectedObjectDetectionModel = null;
        this._selectedCpuObjectDetectionModel = null;
      }
      this._renderForm();
    } catch { /* handled by API */ }
  },

  _renderForm() {
    const el = document.getElementById('config-container');
    if (!el || !this._config) return;

    // Camera resolution/fps, Audio settings, Video Source mode, and
    // Feature flags are intentionally not surfaced here - they remain
    // in config.yaml for ops/automation but are not user-tunable from
    // the UI. Camera and audio defaults match the production hardware
    // (1920x1080 @ 30 fps, alsa default device); video source is
    // implicitly driven by the Replay feature; feature flags are all
    // expected to be on.
    //
    // Telemetry (GameController) and Deployment (Jetson platform)
    // sections ARE surfaced - they're meaningful to flip at runtime
    // per host (gc_test_mode for demos without a live GC;
    // platform=jetson for the Jetson hardware target).
    el.innerHTML = `
      ${this._nodeSection()}
      ${this._objectDetectionModelSection()}
      ${this._config.features?.cpu_detection ? this._cpuObjectDetectionModelSection() : ''}
      ${this._section('Recording', [
        this._field('video.recording.directory', 'Directory', this._config.video.recording.directory, 'text'),
        this._field('video.recording.video_codec', 'Video Codec', this._config.video.recording.video_codec, 'text'),
        this._field('video.recording.audio_codec', 'Audio Codec', this._config.video.recording.audio_codec, 'text'),
        this._checkbox('video.recording.audio_enabled', 'Audio Enabled', this._config.video.recording.audio_enabled),
        // GC-driven auto-recording. `automatic` makes the
        // control_api recording_controller segment recordings by
        // (team1, team2, match_phase) on telemetry.game_state events.
        // Takes effect on the next GC packet - no restart needed.
        this._select('video.recording.recording_mode', 'Recording Mode', this._config.video.recording.recording_mode || 'manual', ['manual', 'automatic']),
      ])}
      ${this._section('Streaming', [
        this._checkbox('video.streaming.enabled', 'Streaming Enabled', this._config.video.streaming.enabled),
        this._select('video.streaming.platform', 'Platform', this._config.video.streaming.platform, ['youtube', 'twitch', 'custom']),
        this._field('video.streaming.rtmp_url', 'RTMP URL', this._config.video.streaming.rtmp_url || '', 'text'),
        this._field('video.streaming.stream_key', 'Stream Key', this._config.video.streaming.stream_key || '', 'password'),
        this._field('video.streaming.bitrate_kbps', 'Bitrate (kbps)', this._config.video.streaming.bitrate_kbps || 2500, 'number'),
        this._field('video.streaming.field_name', 'Field Name (overlay)', this._config.video.streaming.field_name || 'FIELD A', 'text'),
      ])}
      ${this._wifiSection()}
      ${this._section('AI', [
        this._field('ai.accelerator', 'Accelerator', this._config.ai.accelerator, 'text'),
      ])}
      ${this._telemetrySection()}
      ${this._deploymentSection()}
      ${this._themeSection()}
      <div style="margin-top: 1rem; display: flex; gap: 0.5rem;">
        <button class="btn btn-primary" onclick="ConfigPage.save()">Save Configuration</button>
        <button class="btn btn-secondary" onclick="ConfigPage._load()">Reset</button>
      </div>
    `;
  },

  _section(title, fields) {
    return `<div class="card"><h2>${title}</h2>${fields.join('')}</div>`;
  },

  _field(path, label, value, type) {
    const step = type === 'number' ? ' step="any"' : '';
    return `<div class="form-group">
      <label for="cfg-${path}">${label}</label>
      <input type="${type}" id="cfg-${path}" data-path="${path}" value="${value}"${step} />
    </div>`;
  },

  _checkbox(path, label, checked) {
    const chk = checked ? ' checked' : '';
    return `<div class="form-group">
      <label><input type="checkbox" id="cfg-${path}" data-path="${path}"${chk} /> ${label}</label>
    </div>`;
  },

  _select(path, label, value, options) {
    const opts = options.map(o =>
      `<option value="${o}"${o === value ? ' selected' : ''}>${o}</option>`
    ).join('');
    return `<div class="form-group">
      <label for="cfg-${path}">${label}</label>
      <select id="cfg-${path}" data-path="${path}">${opts}</select>
    </div>`;
  },

  _nodeSection() {
    return this._section('Node', [
      this._field('node.id', 'Node ID', this._config.node.id, 'text'),
    ]);
  },

  /**
   * Telemetry section - GameController port + test-mode toggle,
   * backed by the apps/telemetry_service GC listener.
   *
   * `gc_test_mode: true` makes the control_api spawn a synthetic
   * TestSource that publishes random GC packets at 1 Hz, so the
   * cairo overlay renders against realistic data without a live GC
   * on the LAN. Operators flipping this from the UI need to restart
   * the control_api service to apply.
   */
  _telemetrySection() {
    const t = this._config.telemetry || {};
    return this._section('Telemetry (GameController)', [
      this._field(
        'telemetry.game_controller_port',
        'GameController UDP Port',
        t.game_controller_port ?? 3838,
        'number',
      ),
      this._checkbox(
        'telemetry.gc_test_mode',
        'GC Test Mode (synthetic packets at 1 Hz when no live GC)',
        !!t.gc_test_mode,
      ),
    ]);
  },

  /**
   * Field-wifi selector. Dropdown of "none" (wifi off) plus every
   * configured profile name. On save the control_api activates the
   * selected profile on the Pi (nmcli) and locks the wifi interface
   * to receive-only via the firewall. Overlay/GameController data
   * still arrives over the field link; the camera only receives.
   */
  _wifiSection() {
    const fw = (this._config.network && this._config.network.field_wifi) || {};
    const names = (fw.profiles || []).map(p => p.name);
    const options = ['none', ...names];
    const selected = fw.selected_profile || 'none';
    return this._section('Wifi', [
      this._select('network.field_wifi.selected_profile', 'Field Network', selected, options),
    ]);
  },

  /**
   * Deployment section - per-host hardware platform selection.
   * Normally pinned during install; the UI surfaces it so operators
   * can verify what was written and (rarely) override per host
   * without dropping to a shell.
   *
   * `platform: jetson` triggers normalize_for_platform on the
   * control_api side, which force-disables CPU detection and nulls
   * the Hailo model selection (no Hailo on Jetson Nano).
   */
  _deploymentSection() {
    const d = this._config.deployment || {};
    return this._section('Deployment (Hardware Platform)', [
      this._select(
        'deployment.platform',
        'Platform',
        d.platform || 'pi',
        ['pi', 'jetson'],
      ),
      this._select(
        'deployment.camera_backend',
        'Camera Backend',
        d.camera_backend || 'libcamera',
        ['libcamera', 'nvargus', 'v4l2'],
      ),
      this._select(
        'deployment.video_encoder',
        'H.264 Encoder',
        d.video_encoder || 'x264',
        ['x264', 'nvv4l2_h264'],
      ),
    ]);
  },

  _themeSection() {
    const current = Theme.current();
    const opts = ['dark', 'light'].map(t =>
      `<option value="${t}"${t === current ? ' selected' : ''}>${t.charAt(0).toUpperCase() + t.slice(1)}</option>`
    ).join('');
    return `<div class="card">
      <h2>UI Theme</h2>
      <div class="form-group">
        <label for="cfg-theme">Color Theme</label>
        <select id="cfg-theme" onchange="ConfigPage._onThemeChange(this.value)">${opts}</select>
        <span style="font-size:0.75rem;color:var(--text-muted)">Applies to this browser only</span>
      </div>
    </div>`;
  },

  _onThemeChange(name) {
    Theme.set(name);
  },

  _readForm() {
    const config = JSON.parse(JSON.stringify(this._config));
    document.querySelectorAll('[data-path]').forEach(el => {
      const path = el.dataset.path.split('.');
      let obj = config;
      for (let i = 0; i < path.length - 1; i++) {
        if (obj[path[i]] === undefined) obj[path[i]] = {};
        obj = obj[path[i]];
      }
      const key = path[path.length - 1];
      if (el.type === 'checkbox') {
        obj[key] = el.checked;
      } else if (el.dataset.type === 'decimal') {
        obj[key] = parseFloat(el.value.replace(',', '.')) || 0.0;
      } else if (el.type === 'number') {
        const v = parseFloat(el.value);
        obj[key] = Number.isInteger(v) ? parseInt(el.value) : v;
      } else {
        obj[key] = el.value;
      }
    });
    return config;
  },

  _objectDetectionModelSection() {
    return this._modelSection({
      title: 'Object Detection Model (Hailo)',
      models: this._objectDetectionModels || [],
      selected: this._selectedObjectDetectionModel || '',
      elementId: 'cfg-object-detection-model',
      onchange: 'ConfigPage._onObjectDetectionModelChange(this.value)',
    });
  },

  async _onObjectDetectionModelChange(displayName) {
    try {
      await API.put('/models/select', {
        scope: 'object_detection',
        display_name: displayName || null,
      });
      this._selectedObjectDetectionModel = displayName || null;
      Notify.success(
        displayName
          ? `Object detection model (Hailo): ${displayName}`
          : 'Object detection model (Hailo) cleared'
      );
      this._renderForm();
    } catch { /* handled by API */ }
  },

  _cpuObjectDetectionModelSection() {
    return this._modelSection({
      title: 'Object Detection Model (CPU)',
      models: this._cpuObjectDetectionModels || [],
      selected: this._selectedCpuObjectDetectionModel || '',
      elementId: 'cfg-cpu-object-detection-model',
      onchange: 'ConfigPage._onCpuObjectDetectionModelChange(this.value)',
    });
  },

  async _onCpuObjectDetectionModelChange(displayName) {
    try {
      await API.put('/models/select', {
        scope: 'object_detection_cpu',
        display_name: displayName || null,
      });
      this._selectedCpuObjectDetectionModel = displayName || null;
      Notify.success(
        displayName
          ? `Object detection model (CPU): ${displayName}`
          : 'Object detection model (CPU) cleared'
      );
      this._renderForm();
    } catch { /* handled by API */ }
  },

  /**
   * Shared renderer for an AI model selection card. The API surface
   * gives us display_name, input.{width,height,format}, output_format,
   * notes - and deliberately no filenames or internal paths.
   */
  _modelSection({ title, models, selected, elementId, onchange }) {
    const opts = models.map(m => {
      const sel = m.display_name === selected ? ' selected' : '';
      const esc = this._escape(m.display_name);
      return `<option value="${esc}"${sel}>${esc}</option>`;
    }).join('');
    const cur = models.find(m => m.display_name === selected);
    const details = cur
      ? `<div class="det-model-info" style="margin-top:0.5rem">
           <span>${cur.input.width}x${cur.input.height} ${this._escape(cur.input.format)}</span>
           &middot;
           <span style="color:var(--text-muted)">${this._escape(cur.output_format)}</span>
           ${cur.notes ? `<div style="font-size:0.8rem;color:var(--text-muted);margin-top:0.25rem">${this._escape(cur.notes)}</div>` : ''}
         </div>`
      : '';
    return `<div class="card">
      <h2>${title}</h2>
      <div class="form-group">
        <label for="${elementId}">Selected Model</label>
        <select id="${elementId}" onchange="${onchange}">
          <option value=""${!selected ? ' selected' : ''}>- No model selected -</option>
          ${opts}
        </select>
      </div>
      ${details}
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

  async save() {
    try {
      const config = this._readForm();
      this._config = await API.put('/config', config);
      Notify.success('Configuration saved');
      this._renderForm();
    } catch { /* handled by API */ }
  },
};
