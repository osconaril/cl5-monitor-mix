// DOM-only gesture handling: values still flow through the Rust control's
// input handler and server validation. A resize never dispatches an input.
(() => {
  const landscape = () => matchMedia('(orientation: landscape)').matches;
  let gesture = null;
  let nativeRange = null;
  let reorder = null;
  let anchor = null;
  let sideways = landscape();
  let rotating = false;

  function remember() {
    if (rotating || landscape() !== sideways) return;
    const area = document.querySelector('.channel-strips');
    if (!area) { anchor = null; return; }
    const bounds = area.getBoundingClientRect();
    const strip = [...area.children].find(child => {
      const box = child.getBoundingClientRect();
      return sideways ? box.right > bounds.left + 1 : box.bottom > bounds.top + 1;
    });
    anchor = strip?.dataset.channel ?? null;
  }
  function cancel() {
    if (reorder) {
      clearInterval(reorder.timer);
      reorder.target?.removeAttribute('data-drop-target');
      if (reorder.handle.hasPointerCapture(reorder.id))
        reorder.handle.releasePointerCapture(reorder.id);
      reorder = null;
    }
    if (!gesture) return;
    clearTimeout(gesture.timer);
    gesture.fader.removeAttribute('data-adjusting');
    if (gesture.fader.dataset.groupFader) {
      gesture.fader.value = '0';
      gesture.fader.dispatchEvent(new Event('cl5-cancel'));
    }
    if (gesture.fader.hasPointerCapture(gesture.id)) {
      gesture.fader.releasePointerCapture(gesture.id);
    }
    gesture = null;
  }

  document.addEventListener('scroll', event => {
    if (event.target.matches?.('.channel-strips')) remember();
  }, true);
  document.addEventListener('input', event => {
    // Chromium's native touch range can emit input even after pointerdown was
    // prevented. Only our armed gesture may submit pointer-driven changes.
    if (event.isTrusted && nativeRange?.fader === event.target) {
      event.stopImmediatePropagation();
      event.target.value = nativeRange.value;
    }
  }, true);
  function reorderTarget() {
    if (!reorder) return;
    const r = reorder;
    r.target?.removeAttribute('data-drop-target');
    const bounds = r.area.getBoundingClientRect();
    const position = sideways ? r.x : r.y;
    const start = sideways ? bounds.left : bounds.top;
    const end = sideways ? bounds.right : bounds.bottom;
    const delta = position < start + 30 ? -12 : position > end - 30 ? 12 : 0;
    if (sideways) r.area.scrollLeft += delta;
    else r.area.scrollTop += delta;
    const target = document.elementFromPoint(r.x, r.y)?.closest('.channel-strip');
    r.target = target?.parentElement === r.area && !target.classList.contains('group-strip') ? target : null;
    r.target?.setAttribute('data-drop-target', '');
  }
  document.addEventListener('pointerdown', event => {
    const handle = event.target.closest('.reorder-handle');
    if (handle && !handle.disabled && event.isPrimary && event.button === 0) {
      cancel();
      remember();
      event.preventDefault();
      reorder = { handle, id: event.pointerId,
        source: handle.closest('.channel-strip').dataset.channel,
        area: handle.closest('.channel-strips'), x: event.clientX, y: event.clientY,
        target: null };
      handle.setPointerCapture(event.pointerId);
      reorder.timer = setInterval(reorderTarget, 40);
      return;
    }
    const fader = event.target.closest('.send-control input[type=range]');
    if (!fader || fader.disabled || !event.isPrimary || event.button !== 0) return;
    cancel();
    remember();
    // Prevent the native range's jump-to-track behavior. A touch starts moving
    // only after a deliberate motion on the fader axis; a cross-axis swipe
    // remains a scroll and never changes sound.
    event.preventDefault();
    nativeRange = { fader, value: fader.value };
    const area = fader.closest('.channel-strips');
    gesture = {
      fader, area, id: event.pointerId, x: event.clientX, y: event.clientY,
      startX: event.clientX, startY: event.clientY,
      armed: event.pointerType !== 'touch', scrolling: false, vertical: getComputedStyle(fader).writingMode === 'vertical-lr', startValue: Number(fader.value),
    };
    fader.setPointerCapture(event.pointerId);
    if (gesture.armed) fader.setAttribute('data-adjusting', '');
  });
  document.addEventListener('pointermove', event => {
    if (reorder?.id === event.pointerId) {
      event.preventDefault();
      reorder.x = event.clientX;
      reorder.y = event.clientY;
      reorderTarget();
      return;
    }
    const g = gesture;
    if (!g || g.id !== event.pointerId) return;
    event.preventDefault();
    if (!g.armed) {
      const along = g.vertical ? Math.abs(event.clientY - g.startY) : Math.abs(event.clientX - g.startX);
      const across = g.vertical ? Math.abs(event.clientX - g.startX) : Math.abs(event.clientY - g.startY);
      if (along > 6 && along >= across * 1.2) {
        g.armed = true;
        g.startX = event.clientX;
        g.startY = event.clientY;
        g.startValue = Number(g.fader.value);
        g.fader.setAttribute('data-adjusting', '');
      } else if (across > 10) {
        g.scrolling = true;
      }
      if (g.scrolling) {
        if (sideways) g.area.scrollLeft += g.x - event.clientX;
        else g.area.scrollTop += g.y - event.clientY;
      }
    } else if (event.clientX !== g.x || event.clientY !== g.y) {
      const bounds = g.fader.getBoundingClientRect();
      const travel = g.vertical ? (g.startY - event.clientY) / bounds.height
        : (event.clientX - g.startX) / bounds.width;
      const min = Number(g.fader.min), max = Number(g.fader.max);
      g.fader.value = String(Math.max(min, Math.min(max, g.startValue + travel * (max - min))));
      g.fader.dispatchEvent(new Event('input', { bubbles: true }));
      if (nativeRange) nativeRange.value = g.fader.value;
    }
    g.x = event.clientX;
    g.y = event.clientY;
  }, { passive: false });
  for (const type of ['pointerup', 'pointercancel', 'lostpointercapture']) {
    document.addEventListener(type, event => {
      if (reorder?.id === event.pointerId) {
        const source = reorder.source, target = reorder.target?.dataset.channel;
        if (type === 'pointerup' && target !== undefined && source !== target) {
          window.dispatchEvent(new CustomEvent('cl5-reorder', {
            detail: JSON.stringify([Number(source), Number(target), (() => {
              const box = reorder.target.getBoundingClientRect();
              return sideways ? reorder.x > box.left + box.width / 2
                : reorder.y > box.top + box.height / 2;
            })()])
          }));
        }
        cancel();
      }
      if (gesture?.id === event.pointerId) {
        if (type === 'pointerup' && gesture.armed && gesture.fader.dataset.groupFader)
          gesture.fader.dispatchEvent(new Event('change', { bubbles: true }));
        cancel();
      }
      if (type === 'pointerup' || type === 'pointercancel') nativeRange = null;
    });
  }
  window.addEventListener('blur', cancel);
  window.addEventListener('resize', () => {
    cancel();
    const next = landscape();
    if (next === sideways) return;
    sideways = next;
    rotating = true;
    requestAnimationFrame(() => {
      const area = document.querySelector('.channel-strips');
      const strip = anchor === null ? area?.firstElementChild
        : area?.querySelector(`[data-channel="${CSS.escape(anchor)}"]`);
      if (strip && area) {
        const bounds = area.getBoundingClientRect();
        const box = strip.getBoundingClientRect();
        area.scrollLeft = sideways ? area.scrollLeft + box.left - bounds.left : 0;
        area.scrollTop = sideways ? 0 : area.scrollTop + box.top - bounds.top;
      }
      requestAnimationFrame(() => { rotating = false; remember(); });
    });
  });
})();
