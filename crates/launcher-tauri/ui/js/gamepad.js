// Controller (W3C Gamepad API) + Keyboard Spatial Navigation for OpenSync Launcher
import { icon } from './icons.js';

const FOCUSABLE_SELECTOR = [
  '.steam-card',
  'button:not(:disabled)',
  'a[href]',
  'input:not(:disabled):not([type="hidden"])',
  'select:not(:disabled)',
  '[tabindex="0"]',
].join(', ');

const STICK_DEADZONE = 0.48;
const SCROLL_DEADZONE = 0.35;
const REPEAT_INITIAL_MS = 260;
const REPEAT_INTERVAL_MS = 115;

export function createGamepadController(hooks) {
  const barEl = document.getElementById('gamepad-bar');

  let rafId = null;
  let connectedPadName = null;
  let inputMode = 'pointer'; // 'pointer' | 'gamepad' | 'keyboard'
  let focusedEl = null;

  // Button edge detection + directional repeat state
  const prevButtons = new Map();
  const dirState = {
    activeDir: null,
    nextFireAt: 0,
  };

  function isVisible(el) {
    if (!el || !el.isConnected) return false;
    if (el.hidden || el.closest('[hidden]')) return false;
    // Skip nested button inside a steam-card during spatial nav so cards move cleanly cell-to-cell
    if (el.classList.contains('card-center-btn')) return false;
    const style = window.getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden' || Number(style.opacity) === 0) {
      return false;
    }
    const rect = el.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }

  function getActiveScope() {
    const lightbox = document.getElementById('lightbox-root');
    if (lightbox && lightbox.children.length > 0) return lightbox;
    const modal = document.getElementById('modal-root');
    if (modal && modal.children.length > 0) return modal;
    const menu = document.querySelector('.profile-menu');
    if (menu && isVisible(menu)) return menu;
    return document.getElementById('app-shell') || document.body;
  }

  function getFocusableList(scope = getActiveScope()) {
    return [...scope.querySelectorAll(FOCUSABLE_SELECTOR)].filter(isVisible);
  }

  function clearFocusClass() {
    if (focusedEl) {
      focusedEl.classList.remove('gp-focused');
      focusedEl = null;
    }
    document.querySelectorAll('.gp-focused').forEach((el) => el.classList.remove('gp-focused'));
  }

  function setFocus(el, { scroll = true } = {}) {
    if (!el || !isVisible(el)) return;
    clearFocusClass();
    focusedEl = el;
    el.classList.add('gp-focused');
    try {
      el.focus({ preventScroll: true });
    } catch {
      // ignore unfocusable edge cases
    }
    if (scroll) {
      el.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'smooth' });
    }
    renderBar();
  }

  function ensureFocus() {
    const scope = getActiveScope();
    if (focusedEl && scope.contains(focusedEl) && isVisible(focusedEl)) {
      return focusedEl;
    }
    const candidates = getFocusableList(scope);
    if (!candidates.length) return null;

    // Prefer a steam-card or primary action inside #view before topbar links
    const preferred =
      candidates.find((el) => el.classList.contains('steam-card'))
      || candidates.find((el) => el.closest('#view') && el.classList.contains('btn-primary'))
      || candidates.find((el) => el.closest('#view'))
      || candidates[0];
    setFocus(preferred);
    return preferred;
  }

  function moveSpatial(dir) {
    const scope = getActiveScope();
    const candidates = getFocusableList(scope);
    if (!candidates.length) return;

    const current = focusedEl && scope.contains(focusedEl) && isVisible(focusedEl)
      ? focusedEl
      : null;

    if (!current) {
      ensureFocus();
      return;
    }

    const cur = current.getBoundingClientRect();
    const cx = cur.left + cur.width / 2;
    const cy = cur.top + cur.height / 2;

    let best = null;
    let bestScore = Infinity;

    for (const cand of candidates) {
      if (cand === current) continue;
      const r = cand.getBoundingClientRect();
      const tx = r.left + r.width / 2;
      const ty = r.top + r.height / 2;
      const dx = tx - cx;
      const dy = ty - cy;

      let primary = 0;
      let cross = 0;
      if (dir === 'right') {
        if (dx <= 6) continue;
        primary = dx;
        cross = Math.abs(dy);
      } else if (dir === 'left') {
        if (dx >= -6) continue;
        primary = -dx;
        cross = Math.abs(dy);
      } else if (dir === 'down') {
        if (dy <= 6) continue;
        primary = dy;
        cross = Math.abs(dx);
      } else if (dir === 'up') {
        if (dy >= -6) continue;
        primary = -dy;
        cross = Math.abs(dx);
      }

      // Strongly prefer elements aligned on the same row/column
      const score = primary + cross * 2.4;
      if (score < bestScore) {
        bestScore = score;
        best = cand;
      }
    }

    if (best) {
      setFocus(best);
    }
  }

  function activateFocused() {
    const el = ensureFocus();
    if (!el) return;

    if (el.classList.contains('steam-card')) {
      const gameId = Number(el.dataset.gameId);
      if (Number.isFinite(gameId)) {
        hooks.onCardPrimary?.(gameId);
        return;
      }
    }

    if (el.tagName === 'SELECT') {
      // Cycle select options smoothly for controller users
      if (el.options.length > 0) {
        el.selectedIndex = (el.selectedIndex + 1) % el.options.length;
        el.dispatchEvent(new Event('change', { bubbles: true }));
      }
      return;
    }

    if (el.tagName === 'INPUT') {
      el.focus();
      return;
    }

    el.click();
  }

  function secondaryActionFocused() {
    const el = focusedEl && isVisible(focusedEl) ? focusedEl : null;
    if (el && el.classList.contains('steam-card')) {
      const gameId = Number(el.dataset.gameId);
      if (Number.isFinite(gameId)) {
        hooks.onCardDetails?.(gameId);
        return;
      }
    }
    const dlCard = el?.closest('.dl-card');
    if (dlCard) {
      const toggle = dlCard.querySelector('[data-dl-pause], [data-dl-resume]');
      if (toggle) {
        toggle.click();
        return;
      }
    }
    hooks.onSecondaryAction?.();
  }

  function backAction() {
    hooks.onBack?.();
  }

  function yAction() {
    hooks.onYAction?.();
  }

  function setInputMode(mode) {
    if (inputMode === mode) return;
    inputMode = mode;
    document.body.classList.toggle('controller-mode', mode === 'gamepad' || mode === 'keyboard');
    if (mode === 'pointer') {
      clearFocusClass();
    }
    renderBar();
  }

  function getContextHints() {
    const modalOpen = (document.getElementById('modal-root')?.children.length || 0) > 0
      || (document.getElementById('lightbox-root')?.children.length || 0) > 0;
    if (modalOpen) {
      return [
        { key: 'A', label: 'Confirm' },
        { key: 'B', label: 'Close' },
      ];
    }

    const view = hooks.getView?.() || 'library';
    const onCard = focusedEl?.classList.contains('steam-card');
    if (view === 'library') {
      if (onCard) {
        const primaryLabel = focusedEl.dataset.primaryAction === 'play'
          ? 'Play'
          : focusedEl.dataset.primaryAction === 'downloads'
            ? 'View download'
            : 'Install';
        return [
          { key: 'A', label: primaryLabel },
          { key: 'X', label: 'Details' },
          { key: 'Y', label: 'Search' },
          { key: 'LB/RB', label: 'Switch tab' },
        ];
      }
      return [
        { key: 'A', label: 'Select' },
        { key: 'Y', label: 'Search' },
        { key: 'LB/RB', label: 'Switch tab' },
      ];
    }

    if (view === 'game-detail') {
      return [
        { key: 'A', label: 'Select' },
        { key: 'X', label: 'Sync saves' },
        { key: 'B', label: 'Back to library' },
      ];
    }

    if (view === 'downloads') {
      return [
        { key: 'A', label: 'Select' },
        { key: 'X', label: 'Pause / Resume' },
        { key: 'LB/RB', label: 'Switch tab' },
      ];
    }

    if (view === 'proton') {
      return [
        { key: 'A', label: 'Select' },
        { key: 'Y', label: 'Rescan' },
        { key: 'LB/RB', label: 'Switch tab' },
      ];
    }

    return [
      { key: 'A', label: 'Select' },
      { key: 'LB/RB', label: 'Switch tab' },
    ];
  }

  function renderBar() {
    if (!barEl) return;
    const shouldShow = Boolean(connectedPadName) || inputMode === 'gamepad' || inputMode === 'keyboard';
    barEl.hidden = !shouldShow;
    document.body.classList.toggle('has-gamepad-bar', shouldShow);
    if (!shouldShow) return;

    const hints = getContextHints();
    const hintsHtml = hints
      .map((h) => `<span class="gp-hint"><kbd class="gp-glyph">${h.key}</kbd><span>${h.label}</span></span>`)
      .join('');

    const statusText = connectedPadName
      ? `Controller connected`
      : 'Keyboard navigation';

    barEl.innerHTML = `
      <div class="gp-bar-inner">
        <div class="gp-status">
          ${icon('gamepad-2', 15)}
          <span>${statusText}</span>
        </div>
        <div class="gp-hints">${hintsHtml}</div>
      </div>
    `;
  }

  function pollGamepads(now) {
    const pads = navigator.getGamepads ? navigator.getGamepads() : [];
    let activePad = null;
    for (const p of pads) {
      if (p && p.connected) {
        activePad = p;
        break;
      }
    }

    if (activePad) {
      if (!connectedPadName) {
        connectedPadName = activePad.id || 'Controller';
        setInputMode('gamepad');
        ensureFocus();
      }

      const pressed = (idx) => Boolean(activePad.buttons[idx]?.pressed);
      const justPressed = (idx) => {
        const cur = pressed(idx);
        const prev = Boolean(prevButtons.get(idx));
        prevButtons.set(idx, cur);
        return cur && !prev;
      };

      // Directional input (D-pad + left stick)
      const lx = activePad.axes[0] || 0;
      const ly = activePad.axes[1] || 0;
      let dir = null;
      if (pressed(12) || ly < -STICK_DEADZONE) dir = 'up';
      else if (pressed(13) || ly > STICK_DEADZONE) dir = 'down';
      else if (pressed(14) || lx < -STICK_DEADZONE) dir = 'left';
      else if (pressed(15) || lx > STICK_DEADZONE) dir = 'right';

      if (dir) {
        setInputMode('gamepad');
        if (dirState.activeDir !== dir) {
          dirState.activeDir = dir;
          dirState.nextFireAt = now + REPEAT_INITIAL_MS;
          moveSpatial(dir);
        } else if (now >= dirState.nextFireAt) {
          dirState.nextFireAt = now + REPEAT_INTERVAL_MS;
          moveSpatial(dir);
        }
      } else {
        dirState.activeDir = null;
      }

      // Right stick smooth scrolling
      const ry = activePad.axes[3] || 0;
      if (Math.abs(ry) > SCROLL_DEADZONE) {
        window.scrollBy({ top: ry * 14, behavior: 'auto' });
      }

      // Face & shoulder buttons
      if (justPressed(0)) {
        setInputMode('gamepad');
        activateFocused();
      }
      if (justPressed(1)) {
        setInputMode('gamepad');
        backAction();
      }
      if (justPressed(2)) {
        setInputMode('gamepad');
        secondaryActionFocused();
      }
      if (justPressed(3)) {
        setInputMode('gamepad');
        yAction();
      }
      if (justPressed(4)) {
        setInputMode('gamepad');
        hooks.onCycleTab?.(-1);
      }
      if (justPressed(5)) {
        setInputMode('gamepad');
        hooks.onCycleTab?.(1);
      }
      if (justPressed(9)) {
        setInputMode('gamepad');
        hooks.onToggleProfileMenu?.();
      }
    } else if (connectedPadName) {
      connectedPadName = null;
      prevButtons.clear();
      dirState.activeDir = null;
      renderBar();
    }

    rafId = window.requestAnimationFrame(pollGamepads);
  }

  // Pointer usage relaxes visual focus ring unless a controller stays connected
  window.addEventListener('pointerdown', () => {
    if (!connectedPadName) {
      setInputMode('pointer');
    } else {
      clearFocusClass();
    }
  }, { passive: true });

  // Keyboard spatial navigation & shortcuts matching controller controls
  window.addEventListener('keydown', (e) => {
    const tag = document.activeElement?.tagName;
    const typing = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT';

    if (e.key === 'Escape') {
      backAction();
      return;
    }

    if (typing) {
      if (e.key === 'ArrowDown' && tag === 'INPUT') {
        e.preventDefault();
        setInputMode('keyboard');
        moveSpatial('down');
      }
      return;
    }

    if (e.key === 'ArrowUp' || e.key === 'ArrowDown' || e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
      e.preventDefault();
      setInputMode('keyboard');
      const map = { ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left', ArrowRight: 'right' };
      moveSpatial(map[e.key]);
      return;
    }

    if (e.key === 'Enter' || e.key === ' ') {
      if (focusedEl && focusedEl.classList.contains('steam-card')) {
        e.preventDefault();
        activateFocused();
      }
      return;
    }

    if (e.key === 'i' || e.key === 'I') {
      if (focusedEl && focusedEl.classList.contains('steam-card')) {
        e.preventDefault();
        secondaryActionFocused();
      }
      return;
    }

    if (e.key === '[') {
      e.preventDefault();
      setInputMode('keyboard');
      hooks.onCycleTab?.(-1);
      return;
    }

    if (e.key === ']') {
      e.preventDefault();
      setInputMode('keyboard');
      hooks.onCycleTab?.(1);
      return;
    }

    if (e.key === '/') {
      e.preventDefault();
      yAction();
    }
  });

  window.addEventListener('gamepadconnected', (e) => {
    connectedPadName = e.gamepad?.id || 'Controller';
    setInputMode('gamepad');
    ensureFocus();
    hooks.onGamepadConnected?.(connectedPadName);
  });

  window.addEventListener('gamepaddisconnected', () => {
    const pads = navigator.getGamepads ? navigator.getGamepads() : [];
    const stillConnected = [...pads].find((p) => p && p.connected);
    connectedPadName = stillConnected ? (stillConnected.id || 'Controller') : null;
    if (!connectedPadName) {
      setInputMode('pointer');
    } else {
      renderBar();
    }
  });

  rafId = window.requestAnimationFrame(pollGamepads);

  return {
    onViewRendered() {
      if (connectedPadName || inputMode === 'gamepad' || inputMode === 'keyboard') {
        requestAnimationFrame(() => ensureFocus());
      } else {
        renderBar();
      }
    },
    destroy() {
      if (rafId != null) cancelAnimationFrame(rafId);
    },
  };
}
