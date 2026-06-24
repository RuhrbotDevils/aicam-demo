// Implements frontend logic used by the application.
// Author: Thomas Klute

/**
 * Preview-tile magnifier widget.
 *
 * Magnifier.attach(container, options) binds mouse + touch handlers
 * to a preview container so the operator can press-and-drag to see
 * the region under the cursor at 1:1 native pixel resolution. The
 * container's <img> element is mirrored into a popup positioned at
 * the cursor; the popup hides on mouse-up / touch-end.
 *
 * Reusable across the Dashboard, Object Detection, and Recording
 * preview tiles. The widget is idempotent - a second attach() on
 * the same container is a no-op.
 */
const Magnifier = {
  /**
   * Attach the magnifier to *container*. The container must hold (or
   * eventually hold - the widget tolerates an empty container at
   * attach time) an <img> child whose `naturalWidth` /
   * `naturalHeight` reflect the camera's native resolution. Live
   * previews replace the <img> src as new frames arrive; the popup
   * mirrors the latest src on every move event so the magnified
   * view animates with the underlying preview.
   *
   * @param {HTMLElement} container - the preview-container element.
   * @param {{lensSize?: number}} [options] - lensSize defaults to 240 px.
   */
  attach(container, options = {}) {
    if (!container || container._magnifierAttached) return;
    container._magnifierAttached = true;

    const lensSize = options.lensSize || 240;
    container.classList.add('magnifier-target');

    // Single popup per attach. Created lazily on first show so we
    // don't pay for a hidden DOM node on pages that never magnify.
    let popup = null;
    let popupImg = null;

    const ensurePopup = () => {
      if (popup) return;
      popup = document.createElement('div');
      popup.className = 'magnifier-popup';
      popup.style.width = `${lensSize}px`;
      popup.style.height = `${lensSize}px`;
      popupImg = document.createElement('img');
      popupImg.className = 'magnifier-popup-img';
      popup.appendChild(popupImg);
      document.body.appendChild(popup);
    };

    const showAt = (clientX, clientY) => {
      const img = container.querySelector('img');
      if (!img || !img.src || !img.complete) return;
      const rect = img.getBoundingClientRect();
      // Reject events that aren't actually over the visible image.
      if (
        clientX < rect.left ||
        clientX > rect.right ||
        clientY < rect.top ||
        clientY > rect.bottom
      ) {
        hide();
        return;
      }

      ensurePopup();

      // Map the cursor's display-space position onto the source
      // image's native pixel coordinates. naturalWidth / Height come
      // from the loaded image; rect.width / height are the rendered
      // size.
      const natW = img.naturalWidth || rect.width;
      const natH = img.naturalHeight || rect.height;
      const sx = ((clientX - rect.left) / rect.width) * natW;
      const sy = ((clientY - rect.top) / rect.height) * natH;

      // Mirror the latest src so live previews animate inside the
      // popup. Skip the assignment when src hasn't changed to avoid
      // re-decoding the same frame on every mousemove tick.
      if (popupImg.src !== img.src) {
        popupImg.src = img.src;
      }
      // Render at 1:1 native pixels - lock width/height to the
      // image's natural size and offset so the cursor pixel sits at
      // the lens centre.
      popupImg.style.width = `${natW}px`;
      popupImg.style.height = `${natH}px`;
      popupImg.style.left = `${-sx + lensSize / 2}px`;
      popupImg.style.top = `${-sy + lensSize / 2}px`;

      // Position the popup *opposite* the cursor's
      // quadrant within the image, so it never covers the area the
      // operator is trying to inspect.
      //
      //   cursor on left half  -> popup right of cursor
      //   cursor on right half -> popup left of cursor
      //   cursor on top half   -> popup below cursor
      //   cursor on bottom half -> popup above cursor
      //
      // px / py are the cursor's normalised position inside the
      // image rect (0..1).
      const margin = 24;
      const px = (clientX - rect.left) / rect.width;
      const py = (clientY - rect.top) / rect.height;
      const onLeftHalf = px < 0.5;
      const onTopHalf = py < 0.5;
      let left = onLeftHalf ? clientX + margin : clientX - lensSize - margin;
      let top = onTopHalf ? clientY + margin : clientY - lensSize - margin;
      // Viewport clamp as the safety net - never let the lens fall
      // off-screen at extreme aspect ratios.
      if (left + lensSize > window.innerWidth) {
        left = window.innerWidth - lensSize;
      }
      if (top + lensSize > window.innerHeight) {
        top = window.innerHeight - lensSize;
      }
      popup.style.left = `${Math.max(0, left)}px`;
      popup.style.top = `${Math.max(0, top)}px`;
      popup.style.display = 'block';
    };

    const hide = () => {
      if (popup) {
        popup.style.display = 'none';
      }
    };

    // Mouse: press-and-hold while pointer over the preview.
    container.addEventListener('mousedown', (e) => {
      if (e.button !== 0) return; // left-button only
      showAt(e.clientX, e.clientY);
      e.preventDefault();
    });
    container.addEventListener('mousemove', (e) => {
      if (popup && popup.style.display !== 'none') {
        showAt(e.clientX, e.clientY);
      }
    });
    // mouseup is bound on document so a release anywhere on the page
    // hides the popup (operator may drag off the tile).
    document.addEventListener('mouseup', hide);
    container.addEventListener('mouseleave', hide);

    // Touch: tap-and-hold equivalent. preventDefault on touchmove so
    // the page doesn't scroll while the operator is dragging the
    // magnifier across a preview tile.
    container.addEventListener(
      'touchstart',
      (e) => {
        const t = e.touches[0];
        if (!t) return;
        showAt(t.clientX, t.clientY);
        e.preventDefault();
      },
      { passive: false },
    );
    container.addEventListener(
      'touchmove',
      (e) => {
        if (!popup || popup.style.display === 'none') return;
        const t = e.touches[0];
        if (!t) return;
        showAt(t.clientX, t.clientY);
        e.preventDefault();
      },
      { passive: false },
    );
    container.addEventListener('touchend', hide);
    container.addEventListener('touchcancel', hide);
  },
};
