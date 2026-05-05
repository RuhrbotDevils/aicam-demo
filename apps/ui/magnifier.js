// Implements frontend logic used by the application.
// Author: Thomas Klute

/**
 * Preview-tile magnifier widget.
 *
 * Usage:
 *   Magnifier.attach(containerEl, { lensSize: 240 });
 *
 * Behaviour:
 *   - Hover: cursor changes to a zoom-in glyph.
 *   - mousedown / touchstart: a small popup appears next to the
 *     pointer, showing the region under the pointer at the
 *     underlying <img>'s native resolution.
 *   - mousemove / touchmove (while pressed): popup tracks the pointer.
 *   - mouseup / touchend / touchcancel: popup hides.
 *
 * Live preview support: the inner image's `src` is refreshed on
 * each move, so when the underlying preview JPEG rotates the lens
 * picks up the new content automatically.
 */
const Magnifier = {
  /**
   * @param {HTMLElement} container - the preview tile (must contain an <img>).
   * @param {object} [options]
   * @param {number} [options.lensSize=240] - popup edge length, px.
   */
  attach(container, options) {
    if (!container || container._magnifierAttached) return;
    container._magnifierAttached = true;

    const lensSize = (options && options.lensSize) || 240;
    container.classList.add("magnifier-target");

    let popup = null;

    const positionPopup = (clientX, clientY) => {
      const img = container.querySelector("img");
      if (!img || !img.naturalWidth) return;

      if (!popup) {
        popup = document.createElement("div");
        popup.className = "magnifier-popup";
        popup.style.width = `${lensSize}px`;
        popup.style.height = `${lensSize}px`;
        const inner = document.createElement("img");
        inner.alt = "";
        inner.draggable = false;
        popup.appendChild(inner);
        document.body.appendChild(popup);
      }

      const rect = img.getBoundingClientRect();
      // Normalised pointer position inside the displayed <img>, 0..1.
      const px = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
      const py = Math.max(0, Math.min(1, (clientY - rect.top) / rect.height));

      // Quadrant-based positioning: place the lens opposite the cursor's
      // half of the image so it does not cover the area being inspected.
      // Left half → lens on the right; right half → lens on the left.
      // Top half → lens below; bottom half → lens above.
      const offset = 24;
      const onLeftHalf = px < 0.5;
      const onTopHalf = py < 0.5;
      const rawLeft = onLeftHalf ? clientX + offset : clientX - lensSize - offset;
      const rawTop = onTopHalf ? clientY + offset : clientY - lensSize - offset;
      // Clamp to the viewport so the lens never falls off-screen, e.g.
      // when the preview tile sits very near a window edge.
      const left = Math.max(8, Math.min(window.innerWidth - lensSize - 8, rawLeft));
      const top = Math.max(8, Math.min(window.innerHeight - lensSize - 8, rawTop));
      popup.style.left = `${left}px`;
      popup.style.top = `${top}px`;

      const inner = popup.firstChild;
      // Refresh src in case the live preview just rotated.
      if (inner.src !== img.src) inner.src = img.src;
      // Native pixel size - the inner img is positioned so the cursor's
      // pixel sits at the centre of the lens.
      inner.style.width = `${img.naturalWidth}px`;
      inner.style.height = `${img.naturalHeight}px`;
      inner.style.left = `${lensSize / 2 - px * img.naturalWidth}px`;
      inner.style.top = `${lensSize / 2 - py * img.naturalHeight}px`;
    };

    const hide = () => {
      if (popup) {
        popup.remove();
        popup = null;
      }
    };

    container.addEventListener("mousedown", (e) => {
      e.preventDefault();
      positionPopup(e.clientX, e.clientY);
    });
    container.addEventListener("mousemove", (e) => {
      if (popup) positionPopup(e.clientX, e.clientY);
    });
    document.addEventListener("mouseup", hide);
    container.addEventListener("mouseleave", hide);

    container.addEventListener(
      "touchstart",
      (e) => {
        const t = e.touches[0];
        if (!t) return;
        positionPopup(t.clientX, t.clientY);
        e.preventDefault();
      },
      { passive: false },
    );
    container.addEventListener(
      "touchmove",
      (e) => {
        if (!popup) return;
        const t = e.touches[0];
        if (!t) return;
        positionPopup(t.clientX, t.clientY);
        e.preventDefault();
      },
      { passive: false },
    );
    container.addEventListener("touchend", hide);
    container.addEventListener("touchcancel", hide);
  },
};
