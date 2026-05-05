// Implements client-side API calls and related data handling.
// Author: Thomas Klute

/**
 * API helper - wraps fetch calls to /api/v1/* with error handling.
 */
const API = {
  base: '/api/v1',

  async get(path) {
    try {
      const res = await fetch(`${this.base}${path}`);
      if (!res.ok) {
        const err = await this._httpError(res);
        Notify.error(`API error: ${err.message}`);
        throw err;
      }
      if (res.status === 204) return null;
      return await res.json();
    } catch (err) {
      if (!err._apiHandled) Notify.error(`API error: ${err.message}`);
      throw err;
    }
  },

  async post(path, body = null) {
    try {
      const opts = { method: 'POST' };
      if (body) {
        opts.headers = { 'Content-Type': 'application/json' };
        opts.body = JSON.stringify(body);
      }
      const res = await fetch(`${this.base}${path}`, opts);
      if (!res.ok) {
        const err = await this._httpError(res);
        Notify.error(`API error: ${err.message}`);
        throw err;
      }
      return await res.json();
    } catch (err) {
      if (!err._apiHandled) Notify.error(`API error: ${err.message}`);
      throw err;
    }
  },

  async delete(path) {
    try {
      const res = await fetch(`${this.base}${path}`, { method: 'DELETE' });
      if (!res.ok) {
        const err = await this._httpError(res);
        Notify.error(`API error: ${err.message}`);
        throw err;
      }
      return await res.json();
    } catch (err) {
      if (!err._apiHandled) Notify.error(`API error: ${err.message}`);
      throw err;
    }
  },

  // Build an Error that carries the JSON detail field (if present) from a
  // non-2xx response. The error's .detail property holds the server message;
  // .message is human-readable for Notify.error.
  async _httpError(res) {
    let detail = `${res.status} ${res.statusText}`;
    try {
      const body = await res.json();
      if (body && body.detail) detail = body.detail;
    } catch { /* non-JSON body - keep generic message */ }
    const err = new Error(detail);
    err.detail = detail;
    err._apiHandled = true;
    return err;
  },

  async put(path, body) {
    try {
      const res = await fetch(`${this.base}${path}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        const err = await this._httpError(res);
        Notify.error(`API error: ${err.message}`);
        throw err;
      }
      return await res.json();
    } catch (err) {
      if (!err._apiHandled) Notify.error(`API error: ${err.message}`);
      throw err;
    }
  },
};

/**
 * Notification helper - shows success/error toasts.
 */
const Notify = {
  _el: null,
  _timeout: null,

  _show(msg, cls) {
    if (!this._el) this._el = document.getElementById('notification');
    this._el.textContent = msg;
    this._el.className = `notification ${cls}`;
    clearTimeout(this._timeout);
    this._timeout = setTimeout(() => {
      this._el.className = 'notification hidden';
    }, 3000);
  },

  success(msg) { this._show(msg, 'success'); },
  error(msg) { this._show(msg, 'error'); },
};
