import { bridge, listenEvents } from './bridge.js';
import { createGamepadController } from './gamepad.js';
import { icon } from './icons.js';
import { WaveformCanvas } from './waveform.js';

// ---------- Formatting & URL Helpers ----------

function esc(str) {
  return String(str ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function normalizeServerUrl(raw) {
  let s = String(raw || '').trim();
  if (!s) return '';
  if (!/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(s)) {
    s = `http://${s}`;
  }
  try {
    const u = new URL(s);
    u.hash = '';
    u.search = '';
    let p = u.pathname.replace(/\/+$/, '');
    if (p === '/api') p = '';
    else if (p.includes('/api/')) p = p.slice(0, p.indexOf('/api/'));
    u.pathname = p || '/';
    return u.toString().replace(/\/+$/, '');
  } catch {
    return s.split('#')[0].split('?')[0].replace(/\/+$/, '');
  }
}

function humanBytes(bytes) {
  const n = Number(bytes || 0);
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let val = n / 1024;
  let u = 0;
  while (val >= 1024 && u < units.length - 1) {
    val /= 1024;
    u++;
  }
  return `${val >= 100 ? val.toFixed(0) : val >= 10 ? val.toFixed(1) : val.toFixed(2)} ${units[u]}`;
}

function humanSpeed(bps) {
  const n = Number(bps || 0);
  if (n <= 0) return '0 MB/s';
  const mb = n / (1024 * 1024);
  if (mb >= 100) return `${mb.toFixed(0)} MB/s`;
  if (mb >= 1) return `${mb.toFixed(1)} MB/s`;
  return `${Math.max(1, Math.round(n / 1024))} KB/s`;
}

function humanEta(secs) {
  if (secs == null || !Number.isFinite(secs) || secs < 0) return 'Estimating…';
  if (secs < 1) return 'Finishing…';
  const s = Math.round(secs);
  if (s < 60) return `${s}s left`;
  const m = Math.floor(s / 60);
  const remS = s % 60;
  if (m < 60) return remS ? `${m}m ${remS}s left` : `${m}m left`;
  const h = Math.floor(m / 60);
  const remM = m % 60;
  return remM ? `${h}h ${remM}m left` : `${h}h left`;
}

function humanPlaytime(totalSecs) {
  const s = Math.max(0, Math.floor(Number(totalSecs || 0)));
  if (s === 0) return 'Not played yet';
  if (s < 60) return `${s}s played`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m played`;
  const h = Math.floor(m / 60);
  const remM = m % 60;
  return remM ? `${h}h ${remM}m played` : `${h}h played`;
}

function humanRelativeDate(iso) {
  if (!iso) return 'Never';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return 'Never';
  const diffSec = Math.max(0, Math.floor((Date.now() - d.getTime()) / 1000));
  if (diffSec < 60) return 'Just now';
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.floor(diffHr / 24);
  if (diffDay < 7) return `${diffDay}d ago`;
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', year: 'numeric' });
}

function monogram(name) {
  const words = String(name || 'OS').trim().split(/\s+/).filter(Boolean);
  if (words.length >= 2) return (words[0][0] + words[1][0]).toUpperCase();
  return String(name || 'OS').trim().slice(0, 2).toUpperCase();
}

function protonSourceLabel(source) {
  switch (source) {
    case 'OpenSyncManaged': return 'OpenSync';
    case 'SteamCompatTools': return 'Steam Compat';
    case 'SteamCommon': return 'Steam';
    case 'FlatpakSteam': return 'Flatpak';
    default: return 'Custom';
  }
}

function parseSystemRequirements(raw) {
  const lines = String(raw || '').split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  const pairs = [];
  const notes = [];
  for (const line of lines) {
    const idx = line.indexOf(':');
    if (idx > 0 && idx < 24) {
      const k = line.slice(0, idx).trim();
      const v = line.slice(idx + 1).trim();
      if (k && v) {
        pairs.push([k, v]);
        continue;
      }
    }
    notes.push(line);
  }
  return { pairs, notes };
}

function normalizeConnectionState(raw) {
  if (!raw) return { status: 'disconnected' };
  if (typeof raw === 'string') return { status: raw.toLowerCase() };
  if (raw.status) return { status: String(raw.status).toLowerCase(), attempt: raw.attempt };
  if (raw.Reconnecting) return { status: 'reconnecting', attempt: raw.Reconnecting.attempt || 1 };
  const key = Object.keys(raw)[0];
  if (key) return { status: key.toLowerCase(), attempt: raw[key]?.attempt };
  return { status: 'disconnected' };
}

function normalizeDownloadJob(raw) {
  if (!raw) return null;
  const kindType = raw.kind?.type === 'proton' || raw.kind === 'ge_proton' ? 'ge_proton' : 'game';
  const gameId = kindType === 'game' ? (raw.kind?.game_id ?? raw.game_id ?? null) : null;
  const fresh = Boolean(raw.kind?.fresh);
  const stage = String(raw.status?.state ?? raw.stage ?? 'queued').toLowerCase();
  const error = raw.status?.error ?? raw.error ?? null;
  const bytesDownloaded = Number(raw.bytes_downloaded || 0);
  const totalBytes = Number(raw.total_bytes || 0);
  const bps = Number(raw.bytes_per_sec || 0);
  const remaining = Math.max(0, totalBytes - bytesDownloaded);
  const etaSecs = stage === 'downloading' && bps > 0
    ? Math.ceil(remaining / bps)
    : stage === 'verifying' || stage === 'completed'
      ? 0
      : null;

  return {
    job_id: raw.id || raw.job_id,
    kind: kindType,
    game_id: gameId,
    title: raw.name || raw.title || 'Download',
    subtitle: kindType === 'ge_proton'
      ? 'GE-Proton compatibility runtime (SHA-512 verified)'
      : fresh
        ? 'Fresh install (clean manifest)'
        : 'Synced archive (STORE wire-speed)',
    stage,
    paused: stage === 'paused',
    bytes_downloaded: bytesDownloaded,
    total_bytes: totalBytes,
    files_extracted: Number(raw.files_extracted || 0),
    total_files: Number(raw.total_files || 0),
    current_file: raw.current_file || null,
    bytes_per_sec: bps,
    eta_secs: etaSecs,
    error,
    speed_samples: Array.isArray(raw.speed_samples) ? raw.speed_samples : [],
  };
}

// ---------- App State ----------

const TAB_ORDER = ['library', 'downloads', 'proton', 'profiles'];

const state = {
  view: 'library', // 'library' | 'game-detail' | 'downloads' | 'proton' | 'profiles'
  selectedGameId: null,
  activeProfile: null,
  profiles: [],
  connectionState: { status: 'disconnected' },
  protonInstallations: [],
  pendingSyncs: [],
  downloads: [],
  bandwidthLimitBps: 0,
  library: [],
  coverCache: new Map(), // game_id -> dataUrl | null
  coverPending: new Set(),
  playingGameIds: new Set(),
  syncingGameIds: new Set(),
  searchQuery: '',
  filterMode: 'all', // 'all' | 'installed'
  sortBy: 'recent', // 'recent' | 'newest' | 'name' | 'size'
  profileMenuOpen: false,
  latestGeProton: null,
  checkingGeProton: false,
  geProtonError: null,
  reconnecting: false,
};

const activeWaveforms = new Map(); // job_id -> WaveformCanvas

// ---------- DOM References ----------

const viewEl = document.getElementById('view');
const profileSwitcherEl = document.getElementById('profile-switcher');
const statusRightEl = document.getElementById('status-right');
const modalRoot = document.getElementById('modal-root');
const lightboxRoot = document.getElementById('lightbox-root');
const toastRoot = document.getElementById('toast-root');
const navDlPill = document.getElementById('nav-dl-pill');

let gamepadCtrl = null;

// ---------- Toast System ----------

function showToast(message, tone = 'accent', iconName = 'check') {
  const el = document.createElement('div');
  el.className = `toast ${tone}`;
  el.innerHTML = `<span class="toast-icon">${icon(iconName, 16)}</span><span>${esc(message)}</span>`;
  toastRoot.appendChild(el);
  setTimeout(() => {
    el.style.transition = 'opacity 0.22s ease, transform 0.22s ease';
    el.style.opacity = '0';
    el.style.transform = 'translateY(6px)';
    setTimeout(() => el.remove(), 240);
  }, 3400);
}

// ---------- Cover Lazy Loader ----------

async function ensureCover(game) {
  if (!game || !game.has_cover) return;
  if (state.coverCache.has(game.game_id) || state.coverPending.has(game.game_id)) return;
  state.coverPending.add(game.game_id);
  try {
    const dataUrl = await bridge.getGameCoverDataUrl(game.game_id);
    state.coverCache.set(game.game_id, dataUrl || null);
    if (dataUrl) {
      document.querySelectorAll(`[data-cover-game="${game.game_id}"]`).forEach((slot) => {
        const img = document.createElement('img');
        img.className = slot.dataset.coverClass || 'card-cover-img';
        img.src = dataUrl;
        img.alt = game.name;
        slot.replaceWith(img);
        img.dataset.coverLoaded = String(game.game_id);
      });
    }
  } catch {
    state.coverCache.set(game.game_id, null);
  } finally {
    state.coverPending.delete(game.game_id);
  }
}

// ---------- Steam 3D Perspective Tilt ----------

function attachSteamTilt(card) {
  const MAX_TILT_X = 7;
  const MAX_TILT_Y = 8;
  card.addEventListener('pointermove', (e) => {
    const rect = card.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    const px = ((e.clientX - rect.left) / rect.width) * 2 - 1;
    const py = ((e.clientY - rect.top) / rect.height) * 2 - 1;
    card.style.setProperty('--rx', `${(-py * MAX_TILT_X).toFixed(2)}deg`);
    card.style.setProperty('--ry', `${(px * MAX_TILT_Y).toFixed(2)}deg`);
    card.style.setProperty('--gx', `${(((e.clientX - rect.left) / rect.width) * 100).toFixed(1)}%`);
    card.style.setProperty('--gy', `${(((e.clientY - rect.top) / rect.height) * 100).toFixed(1)}%`);
  });
  card.addEventListener('pointerleave', () => {
    card.style.removeProperty('--rx');
    card.style.removeProperty('--ry');
    card.style.removeProperty('--gx');
    card.style.removeProperty('--gy');
  });
}

// ---------- Card Hover Stat Ticker (cycles every 2s while hovered or focused) ----------

function formatCardPlaytime(playtime) {
  const secs = Math.max(0, Math.floor(Number(playtime?.total_secs || 0)));
  if (secs === 0) return 'Never played before';

  let recent = false;
  if (playtime?.last_played_at) {
    const ts = new Date(playtime.last_played_at).getTime();
    if (!Number.isNaN(ts) && (Date.now() - ts) <= 14 * 86400 * 1000) {
      recent = true;
    }
  }

  if (secs < 60) {
    return recent ? 'Played recently' : '< 1 min played';
  }
  const mins = Math.floor(secs / 60);
  if (mins < 60) {
    return recent ? `${mins}m played recently` : `${mins}m played`;
  }
  const hours = secs / 3600;
  const hStr = hours >= 10
    ? String(Math.round(hours))
    : hours.toFixed(1).replace(/\.0$/, '');
  const unit = hStr === '1' ? 'hour' : 'hours';
  return recent ? `${hStr} ${unit} played recently` : `${hStr} ${unit} played`;
}

function buildCardStats(g, isPlaying) {
  const stats = [humanBytes(g.total_size)];
  if (isPlaying) {
    stats.push('Playing now');
  }

  const totalSecs = Math.max(0, Math.floor(Number(g.playtime?.total_secs || 0)));
  stats.push(formatCardPlaytime(g.playtime));

  if (totalSecs > 0) {
    if (g.playtime?.last_played_at) {
      stats.push(`Last played ${humanRelativeDate(g.playtime.last_played_at)}`);
    }
    const sessions = Number(g.playtime?.session_count || 0);
    if (sessions > 0) {
      stats.push(`${sessions} ${sessions === 1 ? 'session' : 'sessions'}`);
    }
  } else {
    if (g.download_count > 0) {
      stats.push(`${g.download_count} ${g.download_count === 1 ? 'download' : 'downloads'}`);
    }
    if (g.uploader_name) {
      stats.push(`Uploaded by @${g.uploader_name}`);
    }
  }

  return stats;
}

function attachStatTicker(card, stats, intervalMs = 2000) {
  if (!Array.isArray(stats) || stats.length < 2) return;
  const ticker = card.querySelector('.card-stat-ticker');
  if (!ticker) return;

  let index = 0;
  let timer = null;
  let hovered = false;
  let focused = false;

  const swapTo = (nextIndex, direction = 'up') => {
    if (nextIndex === index) return;
    index = nextIndex;
    card.classList.toggle('is-stat-cycling', index !== 0);

    const outClass = direction === 'down' ? 'is-leaving-down' : 'is-leaving';
    const inClass = direction === 'down' ? 'is-entering-down' : 'is-entering';

    for (const stale of ticker.querySelectorAll('.is-leaving, .is-leaving-down')) {
      stale.remove();
    }

    const current = ticker.querySelector('.card-stat-item');
    const next = document.createElement('span');
    next.className = `card-stat-item ${inClass}`;
    next.textContent = stats[index];
    ticker.appendChild(next);

    if (current) {
      current.classList.remove('is-entering', 'is-entering-down');
      current.classList.add(outClass);
      current.setAttribute('aria-hidden', 'true');
      let removed = false;
      const cleanup = () => {
        if (removed) return;
        removed = true;
        current.remove();
      };
      current.addEventListener('animationend', cleanup, { once: true });
      window.setTimeout(cleanup, 460);
    }

    next.addEventListener('animationend', () => {
      next.classList.remove('is-entering', 'is-entering-down');
    }, { once: true });
  };

  const syncTimer = () => {
    if (hovered || focused) {
      if (timer !== null) return;
      timer = window.setInterval(() => {
        if (!card.isConnected) {
          window.clearInterval(timer);
          timer = null;
          return;
        }
        swapTo((index + 1) % stats.length, 'up');
      }, intervalMs);
    } else {
      if (timer !== null) {
        window.clearInterval(timer);
        timer = null;
      }
      if (index !== 0) {
        swapTo(0, 'down');
      }
    }
  };

  card.addEventListener('pointerenter', () => {
    hovered = true;
    syncTimer();
  });
  card.addEventListener('pointerleave', () => {
    hovered = false;
    syncTimer();
  });
  card.addEventListener('focusin', () => {
    focused = true;
    syncTimer();
  });
  card.addEventListener('focusout', (e) => {
    if (!card.contains(e.relatedTarget)) {
      focused = false;
      syncTimer();
    }
  });
}

// ---------- Topbar Navigation & Status ----------

function initTopbarNav() {
  document.getElementById('brand-logo')?.addEventListener('click', (e) => {
    e.preventDefault();
    navigate('library');
  });

  document.querySelectorAll('.nav-link[data-view]').forEach((link) => {
    link.addEventListener('click', (e) => {
      e.preventDefault();
      navigate(link.dataset.view);
    });
  });
}

function updateNavState() {
  const activeKey = state.view === 'game-detail' ? 'library' : state.view;
  document.querySelectorAll('.nav-link[data-view]').forEach((link) => {
    link.classList.toggle('active', link.dataset.view === activeKey);
  });

  const activeJobs = state.downloads.filter(
    (d) => d.stage === 'downloading' || d.stage === 'paused' || d.stage === 'verifying' || d.stage === 'queued',
  );
  const count = activeJobs.length;
  const allPaused = count > 0 && activeJobs.every((d) => d.stage === 'paused' || d.paused);

  if (!navDlPill) return;
  if (count === 0) {
    navDlPill.hidden = true;
    navDlPill.classList.remove('is-paused');
    return;
  }

  let totalBytes = 0;
  let doneBytes = 0;
  for (const j of activeJobs) {
    totalBytes += Math.max(1, Number(j.total_bytes || 0));
    doneBytes += Math.min(Number(j.total_bytes || 0), Number(j.bytes_downloaded || 0));
  }
  const pct = Math.min(100, Math.round((doneBytes / Math.max(1, totalBytes)) * 100));
  navDlPill.hidden = false;
  navDlPill.classList.toggle('is-paused', allPaused);
  navDlPill.textContent = allPaused ? `${count} paused` : `${pct}%`;
}

function renderStatusStrip() {
  const active = state.activeProfile;
  const chipInner = active
    ? esc(monogram(active.label || active.username))
    : icon('server', 13);
  const labelText = active ? active.label : 'Connect instance';
  const subText = active ? `@${active.username}` : 'Sign in';

  let menuHtml = '';
  if (state.profileMenuOpen) {
    const items = state.profiles.map((p) => {
      const isCurrent = active && p.id === active.id;
      return `
        <button type="button" class="profile-menu-item ${isCurrent ? 'active' : ''}" data-switch-profile="${esc(p.id)}">
          <div>
            <div class="profile-menu-name">${esc(p.label)}</div>
            <div class="profile-menu-url">@${esc(p.username)} ${esc(p.server_url)}</div>
          </div>
          ${isCurrent ? icon('check', 15) : ''}
        </button>
      `;
    }).join('');

    menuHtml = `
      <div class="profile-menu" role="menu">
        ${items || '<div class="small muted" style="padding:8px 10px;">No saved instances yet.</div>'}
        <div class="profile-menu-sep"></div>
        <button type="button" class="profile-menu-item" id="btn-menu-add-profile">
          <span style="display:inline-flex;align-items:center;gap:8px;">${icon('plus', 15)}<span>Add instance</span></span>
        </button>
        <button type="button" class="profile-menu-item" id="btn-menu-manage-profiles">
          <span style="display:inline-flex;align-items:center;gap:8px;">${icon('settings', 15)}<span>Manage instances</span></span>
        </button>
      </div>
    `;
  }

  profileSwitcherEl.innerHTML = `
    <button type="button" class="profile-trigger" id="btn-profile-dropdown" aria-expanded="${state.profileMenuOpen}">
      <span class="profile-avatar-chip">${chipInner}</span>
      <span class="profile-trigger-meta">
        <span class="profile-trigger-label">${esc(labelText)}</span>
        <span class="profile-trigger-sub">${esc(subText)}</span>
      </span>
      ${icon('chevron-down', 15)}
    </button>
    ${menuHtml}
  `;

  document.getElementById('btn-profile-dropdown')?.addEventListener('click', (e) => {
    e.stopPropagation();
    state.profileMenuOpen = !state.profileMenuOpen;
    renderStatusStrip();
  });

  profileSwitcherEl.querySelectorAll('[data-switch-profile]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      const pid = btn.dataset.switchProfile;
      state.profileMenuOpen = false;
      renderStatusStrip();
      if (active && active.id === pid) return;
      try {
        await bridge.switchProfile(pid);
        await refreshAll();
        showToast('Switched OpenSync instance', 'accent', 'check');
      } catch (err) {
        showToast(String(err?.message || err), 'danger', 'alert-triangle');
      }
    });
  });

  document.getElementById('btn-menu-add-profile')?.addEventListener('click', () => {
    state.profileMenuOpen = false;
    renderStatusStrip();
    openConnectModal();
  });

  document.getElementById('btn-menu-manage-profiles')?.addEventListener('click', () => {
    state.profileMenuOpen = false;
    renderStatusStrip();
    navigate('profiles');
  });

  // Connection & pending sync indicators
  const pendingCount = state.pendingSyncs.length;
  const pendingHtml = pendingCount > 0
    ? `
      <button type="button" class="status-pill status-pill-warn" id="btn-flush-pending" title="Upload pending offline saves">
        ${icon('refresh-cw', 13)}
        <span class="num">${pendingCount} pending ${pendingCount === 1 ? 'sync' : 'syncs'}</span>
      </button>
    `
    : '';

  const conn = state.connectionState?.status || 'disconnected';
  const connLabel = conn === 'connected'
    ? 'Connected'
    : conn === 'reconnecting'
      ? `Reconnecting${state.connectionState?.attempt ? ` (${state.connectionState.attempt})` : '…'}`
      : conn === 'connecting'
        ? 'Connecting…'
        : 'Offline';
  const connDotClass = conn === 'connected'
    ? 'connected'
    : conn === 'reconnecting' || conn === 'connecting'
      ? 'reconnecting'
      : 'disconnected';

  const reconnectBtn = conn !== 'connected' && state.activeProfile
    ? `<button type="button" class="btn btn-sm" id="btn-reconnect-now" ${state.reconnecting ? 'disabled' : ''}>${icon('refresh-cw', 13, state.reconnecting ? 'spin' : '')}<span>Reconnect</span></button>`
    : '';

  statusRightEl.innerHTML = `
    ${pendingHtml}
    <div class="status-pill ${conn === 'connected' ? 'is-online' : ''}" title="${state.activeProfile ? esc(state.activeProfile.server_url) : 'Not connected'}">
      <span class="status-dot ${connDotClass}"></span>
      <span>${esc(connLabel)}</span>
    </div>
    ${reconnectBtn}
  `;

  document.getElementById('btn-reconnect-now')?.addEventListener('click', async () => {
    state.reconnecting = true;
    renderStatusStrip();
    try {
      await bridge.reconnectNow();
      await refreshAll();
      showToast('Reconnected to OpenSync server', 'accent', 'wifi');
    } catch (err) {
      showToast(`Reconnect failed: ${err?.message || err}`, 'danger', 'wifi-off');
    } finally {
      state.reconnecting = false;
      renderStatusStrip();
    }
  });

  document.getElementById('btn-flush-pending')?.addEventListener('click', async () => {
    const items = [...state.pendingSyncs];
    if (!items.length) return;
    showToast(`Syncing ${items.length} pending ${items.length === 1 ? 'game' : 'games'}…`, 'accent', 'refresh-cw');
    for (const item of items) {
      try {
        await bridge.syncGameNow(item.game_id, false);
      } catch (err) {
        showToast(`Sync failed: ${err?.message || err}`, 'warn', 'alert-triangle');
      }
    }
    await refreshAll();
  });
}

document.addEventListener('click', () => {
  if (state.profileMenuOpen) {
    state.profileMenuOpen = false;
    renderStatusStrip();
  }
});

// ---------- Navigation & View Lifecycle ----------

function destroyWaveforms() {
  for (const wf of activeWaveforms.values()) {
    wf.destroy();
  }
  activeWaveforms.clear();
}

function navigate(nextView, opts = {}) {
  destroyWaveforms();
  state.view = nextView;
  if (opts.gameId != null) {
    state.selectedGameId = opts.gameId;
  }
  updateNavState();
  renderCurrentView(true);
}

function renderCurrentView(animate = false) {
  if (state.view !== 'downloads') {
    destroyWaveforms();
  }
  if (animate) {
    viewEl.classList.remove('view-switch');
    void viewEl.offsetWidth;
    viewEl.classList.add('view-switch');
  }

  switch (state.view) {
    case 'library':
      renderLibraryView();
      break;
    case 'game-detail':
      renderGameDetailView();
      break;
    case 'downloads':
      renderDownloadsView();
      break;
    case 'proton':
      renderProtonView();
      break;
    case 'profiles':
      renderProfilesView();
      break;
    default:
      renderLibraryView();
  }

  gamepadCtrl?.onViewRendered();
}

// ---------- View 1: Game Library ----------

function findActiveDownloadForGame(gameId) {
  return state.downloads.find(
    (d) => d.kind === 'game' && d.game_id === gameId
      && (d.stage === 'downloading' || d.stage === 'paused' || d.stage === 'verifying' || d.stage === 'queued'),
  ) || null;
}

function renderLibraryView() {
  // First-run experience mirrors OpenSync Web's clean #/login card directly
  if (!state.activeProfile) {
    viewEl.innerHTML = `
      <div class="auth-wrap panel">
        <img class="auth-logo" src="img/opensync.png" alt="OpenSync" />
        <p class="auth-tagline">LAN game archive and save sync</p>
        <div id="first-run-connect-host"></div>
      </div>
    `;
    mountConnectForm(document.getElementById('first-run-connect-host'), {
      fullWidthSubmit: true,
      onSuccess: async () => {
        await refreshAll();
        navigate('library');
      },
    });
    return;
  }

  viewEl.innerHTML = `
    <div class="lib-header">
      <h1 class="page-title">game library</h1>
      <div class="lib-header-controls">
        <div class="search-wrap">
          <span class="search-icon">${icon('search', 15)}</span>
          <input type="text" class="input search-input" id="lib-search" placeholder="Filter by name or uploader…" value="${esc(state.searchQuery)}" aria-label="Filter games" />
          <button type="button" class="search-clear" id="lib-search-clear" aria-label="Clear filter" ${state.searchQuery ? '' : 'hidden'}>${icon('x', 14)}</button>
        </div>

        <div class="lib-right-controls">
          <span class="lib-count faint small num" id="lib-count"></span>
          <div class="segmented" role="tablist" aria-label="Filter by install status">
            <button type="button" role="tab" class="${state.filterMode === 'all' ? 'active' : ''}" data-filter="all">All</button>
            <button type="button" role="tab" class="${state.filterMode === 'installed' ? 'active' : ''}" data-filter="installed">Installed</button>
          </div>

          <select class="select" id="lib-sort" aria-label="Sort games">
            <option value="recent" ${state.sortBy === 'recent' ? 'selected' : ''}>Recently played</option>
            <option value="newest" ${state.sortBy === 'newest' ? 'selected' : ''}>Newest first</option>
            <option value="name" ${state.sortBy === 'name' ? 'selected' : ''}>Name A–Z</option>
            <option value="size" ${state.sortBy === 'size' ? 'selected' : ''}>Largest first</option>
          </select>
        </div>
      </div>
    </div>
    <div id="lib-grid-host"></div>
  `;

  const searchInput = document.getElementById('lib-search');
  const clearBtn = document.getElementById('lib-search-clear');
  const sortSelect = document.getElementById('lib-sort');

  searchInput.addEventListener('input', () => {
    state.searchQuery = searchInput.value;
    clearBtn.hidden = !state.searchQuery.trim();
    renderLibraryGrid();
  });

  clearBtn.addEventListener('click', () => {
    state.searchQuery = '';
    searchInput.value = '';
    clearBtn.hidden = true;
    renderLibraryGrid();
    searchInput.focus();
  });

  sortSelect.addEventListener('change', () => {
    state.sortBy = sortSelect.value;
    renderLibraryGrid();
  });

  viewEl.querySelectorAll('[data-filter]').forEach((btn) => {
    btn.addEventListener('click', () => {
      state.filterMode = btn.dataset.filter;
      viewEl.querySelectorAll('[data-filter]').forEach((b) =>
        b.classList.toggle('active', b.dataset.filter === state.filterMode),
      );
      renderLibraryGrid();
    });
  });

  renderLibraryGrid();
}

function renderLibraryGrid() {
  const host = document.getElementById('lib-grid-host');
  const countEl = document.getElementById('lib-count');
  if (!host) return;

  let items = [...state.library];
  if (state.filterMode === 'installed') {
    items = items.filter((g) => g.installed);
  }
  const q = state.searchQuery.trim().toLowerCase();
  if (q) {
    items = items.filter((g) => {
      const hay = `${g.name} ${g.uploader_name || ''} ${g.description || ''}`.toLowerCase();
      return hay.includes(q);
    });
  }

  items.sort((a, b) => {
    if (state.sortBy === 'name') return a.name.localeCompare(b.name);
    if (state.sortBy === 'size') return (b.total_size || 0) - (a.total_size || 0);
    if (state.sortBy === 'newest') return String(b.created_at || '').localeCompare(String(a.created_at || ''));
    const aPlay = a.playtime?.last_played_at || '';
    const bPlay = b.playtime?.last_played_at || '';
    if (aPlay !== bPlay) return bPlay.localeCompare(aPlay);
    if (a.installed !== b.installed) return a.installed ? -1 : 1;
    return String(b.created_at || '').localeCompare(String(a.created_at || ''));
  });

  if (countEl) {
    if (q || state.filterMode === 'installed') {
      countEl.textContent = `${items.length} of ${state.library.length} games`;
    } else {
      countEl.textContent = `${state.library.length} ${state.library.length === 1 ? 'game' : 'games'}`;
    }
  }

  if (!items.length) {
    host.innerHTML = `
      <div class="empty-state">
        <div class="empty-state-icon">${icon('search', 20)}</div>
        <h3>${state.library.length ? 'No matching games' : 'No games on this server yet'}</h3>
        <p>${q ? `Nothing matched "${esc(state.searchQuery.trim())}".` : state.filterMode === 'installed' ? 'No installed games yet. Switch to All to download games from your server.' : 'Upload games in OpenSync Web to see them ready to install here.'}</p>
        ${q || state.filterMode === 'installed' ? '<button type="button" class="btn btn-sm" id="btn-reset-lib-filter">Show all games</button>' : ''}
      </div>
    `;
    document.getElementById('btn-reset-lib-filter')?.addEventListener('click', () => {
      state.searchQuery = '';
      state.filterMode = 'all';
      renderLibraryView();
    });
    return;
  }

  const grid = document.createElement('div');
  grid.className = 'game-grid';

  for (const g of items) {
    const isPlaying = state.playingGameIds.has(g.game_id);
    const isSyncing = state.syncingGameIds.has(g.game_id);
    const dl = findActiveDownloadForGame(g.game_id);
    const dlPct = dl && dl.total_bytes > 0
      ? Math.min(100, Math.round((dl.bytes_downloaded / dl.total_bytes) * 100))
      : 0;

    const coverUrl = state.coverCache.get(g.game_id);
    let coverHtml;
    if (coverUrl) {
      coverHtml = `<img class="card-cover-img" src="${coverUrl}" alt="${esc(g.name)}" loading="lazy" />`;
    } else {
      coverHtml = `
        <div class="card-cover-placeholder" data-cover-game="${g.game_id}" data-cover-class="card-cover-img" aria-hidden="true">
          <span class="card-cover-monogram">${esc(monogram(g.name))}</span>
        </div>
      `;
      if (g.has_cover) ensureCover(g);
    }

    let topPillHtml = '';
    if (isPlaying) {
      topPillHtml = `<span class="card-top-pill playing"><span class="status-dot connected"></span>Playing</span>`;
    } else if (isSyncing) {
      topPillHtml = `<span class="card-top-pill syncing">${icon('refresh-cw', 12, 'spin')}Syncing saves</span>`;
    } else if (dl) {
      topPillHtml = `<span class="card-top-pill downloading">${dl.paused ? 'Paused' : `${dlPct}%`}</span>`;
    }

    let primaryAction = 'download';
    let centerBtnHtml = '';
    if (isPlaying) {
      primaryAction = 'playing';
      centerBtnHtml = `
        <button type="button" class="card-center-btn is-playing" data-card-action="playing" aria-label="Playing ${esc(g.name)}" title="Playing ${esc(g.name)}" tabindex="-1">
          ${icon('play', 22)}
        </button>
      `;
    } else if (dl) {
      primaryAction = 'downloads';
      centerBtnHtml = `
        <button type="button" class="card-center-btn" data-card-action="downloads" aria-label="View download for ${esc(g.name)}" title="Open Download Manager" tabindex="-1">
          <span class="card-dl-pct">${dlPct}%</span>
        </button>
      `;
    } else if (g.installed) {
      primaryAction = 'play';
      centerBtnHtml = `
        <button type="button" class="card-center-btn play-btn" data-card-action="play" aria-label="Play ${esc(g.name)}" title="Play ${esc(g.name)}" tabindex="-1">
          ${icon('play', 23)}
        </button>
      `;
    } else {
      primaryAction = 'download';
      centerBtnHtml = `
        <button type="button" class="card-center-btn" data-card-action="download" aria-label="Download ${esc(g.name)}" title="Install ${esc(g.name)}" tabindex="-1">
          ${icon('arrow-down', 24)}
        </button>
      `;
    }

    const cardStats = buildCardStats(g, isPlaying);

    let syncDotHtml = '';
    if (g.installed) {
      if (g.pending_sync) {
        syncDotHtml = `<span class="card-sync-indicator" title="Saves waiting to sync"><span class="sync-dot pending"></span><span class="card-sync-label">Pending sync</span></span>`;
      } else if (g.last_synced_at || g.has_overlay) {
        syncDotHtml = `<span class="card-sync-indicator" title="Saves synced"><span class="sync-dot synced"></span><span class="card-sync-label">Synced</span></span>`;
      } else {
        syncDotHtml = `<span class="card-sync-indicator" title="Installed"><span class="sync-dot synced"></span><span class="card-sync-label">Installed</span></span>`;
      }
    } else if (g.has_overlay) {
      syncDotHtml = `<span class="card-sync-indicator" title="Cloud saves available"><span class="sync-dot synced"></span><span class="card-sync-label">Cloud save</span></span>`;
    }

    const card = document.createElement('article');
    card.className = `card steam-card${isPlaying ? ' is-playing' : ''}${dl ? ' is-downloading' : ''}`;
    card.tabIndex = 0;
    card.dataset.gameId = String(g.game_id);
    card.dataset.primaryAction = primaryAction;
    card.setAttribute('aria-label', `${g.name}, ${primaryAction === 'play' ? 'installed' : 'available to install'}`);
    card.innerHTML = `
      ${coverHtml}
      <div class="card-glass-sheen" aria-hidden="true"></div>
      ${topPillHtml}
      ${centerBtnHtml}
      <div class="card-glass-footer">
        <div class="card-glass-info">
          <span class="card-glass-title">${esc(g.name)}</span>
          <div class="card-glass-meta">
            <span class="card-stat-ticker"><span class="card-stat-item">${esc(cardStats[0])}</span></span>
            ${syncDotHtml}
          </div>
        </div>
      </div>
    `;

    card.addEventListener('click', () => {
      navigate('game-detail', { gameId: g.game_id });
    });

    const actionBtn = card.querySelector('[data-card-action]');
    actionBtn?.addEventListener('click', async (e) => {
      e.stopPropagation();
      await runCardPrimaryAction(g);
    });

    attachSteamTilt(card);
    attachStatTicker(card, cardStats, 2000);
    grid.appendChild(card);
  }

  host.replaceChildren(grid);
}

async function runCardPrimaryAction(g) {
  if (!g) return;
  const isPlaying = state.playingGameIds.has(g.game_id);
  const dl = findActiveDownloadForGame(g.game_id);
  if (isPlaying) {
    navigate('game-detail', { gameId: g.game_id });
  } else if (dl) {
    navigate('downloads');
  } else if (g.installed) {
    await handleLaunchGame(g);
  } else {
    await handleStartDownload(g, false);
  }
}

// ---------- View 2: Game Detail ----------

function renderGameDetailView() {
  const g = state.library.find((x) => x.game_id === state.selectedGameId);
  if (!g) {
    navigate('library');
    return;
  }

  const isPlaying = state.playingGameIds.has(g.game_id);
  const isSyncing = state.syncingGameIds.has(g.game_id);
  const dl = findActiveDownloadForGame(g.game_id);
  const dlPct = dl && dl.total_bytes > 0
    ? Math.min(100, Math.round((dl.bytes_downloaded / dl.total_bytes) * 100))
    : 0;

  const coverUrl = state.coverCache.get(g.game_id);
  if (!coverUrl && g.has_cover) ensureCover(g);

  const coverInner = coverUrl
    ? `<img src="${coverUrl}" alt="${esc(g.name)}" /><span class="cover-zoom-hint">Expand</span>`
    : `<div class="card-cover-placeholder" data-cover-game="${g.game_id}" data-cover-class=""><span class="card-cover-monogram">${esc(monogram(g.name))}</span></div>`;

  const isWindows = g.binary_kind === 'WindowsPe';
  const needsProton = isWindows && state.protonInstallations.length === 0;

  let primaryBtnHtml;
  if (isPlaying) {
    primaryBtnHtml = `<button type="button" class="btn btn-playing" disabled>${icon('play', 16)}<span>Playing</span></button>`;
  } else if (dl) {
    primaryBtnHtml = `<button type="button" class="btn btn-primary" id="btn-detail-open-dl">${icon('arrow-down', 16)}<span>${dl.paused ? 'Download paused' : `Downloading ${dlPct}%`}</span></button>`;
  } else if (g.installed) {
    primaryBtnHtml = `<button type="button" class="btn btn-primary" id="btn-detail-play">${icon('play', 16)}<span>Play</span></button>`;
  } else {
    primaryBtnHtml = `<button type="button" class="btn btn-primary" id="btn-detail-install">${icon('arrow-down', 16)}<span>Install (${esc(humanBytes(g.total_size))})</span></button>`;
  }

  const secondaryBtns = [];
  if (g.installed) {
    secondaryBtns.push(`
      <button type="button" class="btn" id="btn-detail-sync" ${isSyncing || isPlaying ? 'disabled' : ''}>
        ${icon('refresh-cw', 15, isSyncing ? 'spin' : '')}
        <span>${isSyncing ? 'Syncing saves…' : 'Sync save'}</span>
      </button>
    `);
    secondaryBtns.push(`
      <button type="button" class="btn btn-danger" id="btn-detail-uninstall" ${isPlaying ? 'disabled' : ''}>
        ${icon('trash-2', 15)}
        <span>Uninstall</span>
      </button>
    `);
  } else if (!dl && g.has_overlay) {
    secondaryBtns.push(`
      <button type="button" class="btn" id="btn-detail-fresh">
        ${icon('hard-drive', 15)}
        <span>Fresh install</span>
      </button>
    `);
  }

  const protonOptions = [
    `<option value="" ${!g.proton_override ? 'selected' : ''}>${isWindows ? (state.protonInstallations[0] ? `Auto (${esc(state.protonInstallations[0].name)})` : 'Auto (Proton required)') : 'Native Linux (default)'}</option>`,
    ...state.protonInstallations.map(
      (p) => `<option value="${esc(p.name)}" ${g.proton_override === p.name ? 'selected' : ''}>${esc(p.name)} (${esc(protonSourceLabel(p.source))})</option>`,
    ),
  ].join('');

  const exeOptions = (g.discovered_executables || []).length
    ? g.discovered_executables.map(
        (ex) => `<option value="${esc(ex.rel_path)}" ${g.executable_rel_path === ex.rel_path ? 'selected' : ''}>${esc(ex.rel_path)} (${ex.kind === 'WindowsPe' ? 'Windows PE' : ex.kind === 'LinuxElf' ? 'Linux ELF' : 'Script'})</option>`,
      ).join('')
    : `<option value="">${g.installed ? (g.executable_rel_path || 'Auto-detect on launch') : 'Available after installation'}</option>`;

  const req = parseSystemRequirements(g.system_requirements);
  const reqHtml = req.pairs.length
    ? `<dl class="req-list">${req.pairs.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join('')}</dl>${req.notes.length ? `<p class="small muted" style="margin-top:10px;">${esc(req.notes.join(' '))}</p>` : ''}`
    : `<p class="muted">${esc(g.system_requirements || 'No system requirements listed.')}</p>`;

  const syncBadge = g.installed
    ? g.pending_sync
      ? `<span class="badge badge-warn">Pending save sync</span>`
      : g.last_synced_at
        ? `<span class="badge badge-accent">Synced ${esc(humanRelativeDate(g.last_synced_at))}</span>`
        : `<span class="badge badge-accent">Installed</span>`
    : g.has_overlay
      ? `<span class="badge badge-info">Cloud save available</span>`
      : `<span class="badge">Archive ready</span>`;

  const runtimeBadge = isWindows
    ? `<span class="badge">${icon('cpu', 12)}Windows via Proton</span>`
    : `<span class="badge">${icon('cpu', 12)}Native Linux</span>`;

  viewEl.innerHTML = `
    <div class="detail-back">
      <button type="button" class="btn btn-ghost btn-sm" id="btn-back-library">${icon('arrow-left', 15)}<span>Back to library</span></button>
    </div>

    <div class="game-detail-header">
      <div class="game-cover-lg" id="detail-cover-box" title="Click to view full cover">
        ${coverInner}
      </div>

      <div class="game-detail-main">
        <h1 class="game-detail-title">${esc(g.name)}</h1>
        <div class="game-detail-badges">
          ${syncBadge}
          ${runtimeBadge}
          <span class="badge">${icon('hard-drive', 12)}${esc(humanBytes(g.total_size))}</span>
          ${g.uploader_name ? `<span class="badge">${icon('user', 12)}@${esc(g.uploader_name)}</span>` : ''}
        </div>

        <div class="game-detail-actions">
          ${primaryBtnHtml}
          ${secondaryBtns.join('')}
        </div>

        <div class="kpi-strip">
          <div class="kpi-card">
            <div class="kpi-label">Total played</div>
            <div class="kpi-value">${esc(humanPlaytime(g.playtime?.total_secs || 0))}</div>
          </div>
          <div class="kpi-card">
            <div class="kpi-label">Sessions</div>
            <div class="kpi-value">${esc(String(g.playtime?.session_count || 0))}</div>
          </div>
          <div class="kpi-card">
            <div class="kpi-label">Last played</div>
            <div class="kpi-value neutral">${esc(humanRelativeDate(g.playtime?.last_played_at))}</div>
          </div>
        </div>
      </div>
    </div>

    <div class="detail-grid-2">
      <section class="panel">
        <h2>Runtime and launch</h2>
        ${needsProton ? `
          <div class="proton-alert-box">
            <div class="proton-alert-text">
              <strong>Proton required</strong><br/>
              <span class="small muted">This Windows game needs a Proton compatibility runtime to run on Linux.</span>
            </div>
            <button type="button" class="btn btn-primary btn-sm" id="btn-detail-install-proton">${icon('arrow-down', 14)}<span>Install GE-Proton</span></button>
          </div>
        ` : ''}
        <div class="field">
          <label for="detail-proton-select">Compatibility runtime</label>
          <select id="detail-proton-select" class="select" ${!g.installed ? 'disabled' : ''}>
            ${protonOptions}
          </select>
        </div>
        <div class="field" style="margin-bottom:0;">
          <label for="detail-exe-select">Launch executable</label>
          <select id="detail-exe-select" class="select" ${!g.installed || !(g.discovered_executables || []).length ? 'disabled' : ''}>
            ${exeOptions}
          </select>
        </div>
      </section>

      <section class="panel">
        <h2>System requirements</h2>
        ${reqHtml}
      </section>
    </div>

    <section class="panel">
      <h2>About</h2>
      <p>${esc(g.description || 'No description provided for this game yet.')}</p>
      ${g.install_dir ? `<p class="small muted" style="margin-top:12px;">Install folder: <code>${esc(g.install_dir)}</code></p>` : ''}
    </section>
  `;

  document.getElementById('btn-back-library')?.addEventListener('click', () => navigate('library'));

  document.getElementById('detail-cover-box')?.addEventListener('click', () => {
    const src = state.coverCache.get(g.game_id);
    if (src) openLightbox(src, g.name);
  });

  document.getElementById('btn-detail-play')?.addEventListener('click', () => handleLaunchGame(g));
  document.getElementById('btn-detail-install')?.addEventListener('click', () => handleStartDownload(g, false));
  document.getElementById('btn-detail-fresh')?.addEventListener('click', () => handleStartDownload(g, true));
  document.getElementById('btn-detail-open-dl')?.addEventListener('click', () => navigate('downloads'));

  document.getElementById('btn-detail-sync')?.addEventListener('click', async () => {
    await handleManualSync(g, false);
  });

  document.getElementById('btn-detail-uninstall')?.addEventListener('click', () => {
    openConfirmModal({
      title: `Uninstall ${g.name}`,
      body: 'This removes local game files from your library folder. Your synced saves remain safely stored on the OpenSync server.',
      confirmLabel: 'Uninstall game',
      danger: true,
      onConfirm: async () => {
        try {
          await bridge.uninstallGame(g.game_id);
          await refreshLibrary();
          showToast(`Uninstalled ${g.name}`, 'accent', 'trash-2');
          renderCurrentView();
        } catch (err) {
          showToast(String(err?.message || err), 'danger', 'alert-triangle');
        }
      },
    });
  });

  document.getElementById('btn-detail-install-proton')?.addEventListener('click', async () => {
    await quickInstallLatestGeProton();
  });

  document.getElementById('detail-proton-select')?.addEventListener('change', async (e) => {
    const val = e.target.value || null;
    try {
      await bridge.setGameProton(g.game_id, val);
      g.proton_override = val;
      showToast(val ? `Runtime set to ${val}` : 'Runtime reset to automatic', 'accent', 'cpu');
    } catch (err) {
      showToast(String(err?.message || err), 'danger', 'alert-triangle');
    }
  });

  document.getElementById('detail-exe-select')?.addEventListener('change', async (e) => {
    const val = e.target.value || null;
    try {
      await bridge.setGameExecutable(g.game_id, val);
      g.executable_rel_path = val;
      showToast('Launch executable updated', 'accent', 'check');
    } catch (err) {
      showToast(String(err?.message || err), 'danger', 'alert-triangle');
    }
  });
}

// ---------- View 3: Download Manager (60fps Waveform) ----------

const BW_PRESETS = [
  { label: 'Unlimited', bps: 0 },
  { label: '100 MB/s', bps: 100 * 1024 * 1024 },
  { label: '50 MB/s', bps: 50 * 1024 * 1024 },
  { label: '25 MB/s', bps: 25 * 1024 * 1024 },
  { label: '10 MB/s', bps: 10 * 1024 * 1024 },
];

function renderDownloadsView() {
  destroyWaveforms();

  const bwOptions = BW_PRESETS.map(
    (p) => `<option value="${p.bps}" ${state.bandwidthLimitBps === p.bps ? 'selected' : ''}>${esc(p.label)}</option>`,
  ).join('');

  viewEl.innerHTML = `
    <div class="page-header-row">
      <h1 class="page-title">downloads</h1>
      <div class="bw-control">
        <label for="dl-bw-select">Bandwidth limit</label>
        <select id="dl-bw-select" class="select">${bwOptions}</select>
      </div>
    </div>
    <div id="dl-list-host"></div>
  `;

  document.getElementById('dl-bw-select')?.addEventListener('change', async (e) => {
    const bps = Number(e.target.value || 0);
    state.bandwidthLimitBps = bps;
    try {
      await bridge.setBandwidthLimit(bps);
      const match = BW_PRESETS.find((p) => p.bps === bps);
      showToast(`Bandwidth cap set to ${match ? match.label : humanSpeed(bps)}`, 'accent', 'settings');
    } catch (err) {
      showToast(String(err?.message || err), 'danger', 'alert-triangle');
    }
  });

  syncDownloadsListDom();
}

function stageBadgeHtml(job) {
  switch (job.stage) {
    case 'downloading':
      return `<span class="badge badge-accent">Downloading</span>`;
    case 'paused':
      return `<span class="badge badge-warn">Paused</span>`;
    case 'verifying':
      return `<span class="badge badge-info">${icon('shield-check', 12)}Verifying</span>`;
    case 'completed':
      return `<span class="badge badge-accent">${icon('check', 12)}Ready</span>`;
    case 'failed':
      return `<span class="badge badge-danger">Failed</span>`;
    default:
      return `<span class="badge">Queued</span>`;
  }
}

function buildDownloadCard(job) {
  const card = document.createElement('section');
  card.className = 'dl-card';
  card.dataset.jobId = job.job_id;
  card.innerHTML = `
    <div class="dl-card-top">
      <div class="dl-title-group">
        <div class="dl-title-row">
          <span class="dl-title">${esc(job.title)}</span>
          <span class="badge">${job.kind === 'ge_proton' ? 'GE-Proton' : 'Game'}</span>
          <span data-dl-stage></span>
        </div>
        <div class="dl-subtitle" data-dl-subtitle>${esc(job.subtitle || '')}</div>
      </div>

      <div class="dl-metrics-actions">
        <div class="dl-speed-block">
          <div class="dl-speed-val" data-dl-speed></div>
          <div class="dl-eta-val" data-dl-eta></div>
        </div>
        <div class="dl-controls" data-dl-controls></div>
      </div>
    </div>

    <div class="dl-waveform-box">
      <canvas class="dl-waveform-canvas"></canvas>
    </div>

    <div class="dl-progress-track">
      <div class="dl-progress-fill" data-dl-bar></div>
    </div>

    <div class="dl-footer-row">
      <span class="dl-current-file" data-dl-file></span>
      <span data-dl-bytes></span>
    </div>
  `;

  const canvas = card.querySelector('.dl-waveform-canvas');
  const wf = new WaveformCanvas(canvas, job.speed_samples || [], job.paused || job.stage === 'paused');
  activeWaveforms.set(job.job_id, wf);

  patchDownloadCard(card, job);
  return card;
}

function patchDownloadCard(card, job) {
  const isPaused = job.paused || job.stage === 'paused';
  const isVerifying = job.stage === 'verifying';
  const isCompleted = job.stage === 'completed';
  const isMoving = job.stage === 'downloading' && !isPaused && Number(job.bytes_per_sec || 0) > 0;
  const pct = job.total_bytes > 0
    ? Math.min(100, (Number(job.bytes_downloaded || 0) / Number(job.total_bytes)) * 100)
    : (isCompleted ? 100 : 0);

  card.classList.toggle('is-active', job.stage === 'downloading' && !isPaused);
  card.classList.toggle('is-paused', isPaused);

  const stageEl = card.querySelector('[data-dl-stage]');
  if (stageEl) stageEl.innerHTML = stageBadgeHtml(job);

  const speedEl = card.querySelector('[data-dl-speed]');
  if (speedEl) {
    speedEl.textContent = isPaused
      ? 'Paused'
      : isVerifying
        ? 'Verifying'
        : isCompleted
          ? 'Completed'
          : humanSpeed(job.bytes_per_sec);
  }

  const etaEl = card.querySelector('[data-dl-eta]');
  if (etaEl) {
    etaEl.textContent = isPaused
      ? 'Transfer paused'
      : isVerifying
        ? 'Checking hashes'
        : isCompleted
          ? `${pct.toFixed(0)}% verified`
          : humanEta(job.eta_secs);
  }

  const bar = card.querySelector('[data-dl-bar]');
  if (bar) {
    bar.style.width = `${pct.toFixed(2)}%`;
    bar.dataset.moving = String(isMoving);
    bar.dataset.paused = String(isPaused);
    bar.dataset.verifying = String(isVerifying);
  }

  const fileEl = card.querySelector('[data-dl-file]');
  if (fileEl) {
    fileEl.textContent = job.error
      ? `Error: ${job.error}`
      : job.current_file
        ? `Extracting ${job.current_file}`
        : isCompleted
          ? (job.kind === 'ge_proton' ? 'Installed and verified with SHA-512' : 'Verified and ready to play')
          : 'Waiting for stream…';
  }

  const bytesEl = card.querySelector('[data-dl-bytes]');
  if (bytesEl) {
    const fileCounter = job.total_files > 1 ? `  ${job.files_extracted} of ${job.total_files} files` : '';
    bytesEl.textContent = `${humanBytes(job.bytes_downloaded)} of ${humanBytes(job.total_bytes)} (${pct.toFixed(0)}%)${fileCounter}`;
  }

  const ctrlEl = card.querySelector('[data-dl-controls]');
  if (ctrlEl) {
    const ctrlMode = isCompleted ? 'completed' : isPaused ? 'paused' : isVerifying ? 'verifying' : 'active';
    if (ctrlEl.dataset.ctrlMode !== ctrlMode) {
      ctrlEl.dataset.ctrlMode = ctrlMode;
      if (isCompleted && job.kind === 'game' && job.game_id != null) {
        ctrlEl.innerHTML = `<button type="button" class="btn btn-primary btn-sm" data-dl-play="${job.game_id}">${icon('play', 14)}<span>Play</span></button>`;
        ctrlEl.querySelector('[data-dl-play]')?.addEventListener('click', async () => {
          const g = state.library.find((x) => x.game_id === job.game_id);
          if (g) await handleLaunchGame(g);
        });
      } else if (isCompleted) {
        ctrlEl.innerHTML = `<button type="button" class="btn btn-sm" data-dl-proton>${icon('cpu', 14)}<span>Open Proton</span></button>`;
        ctrlEl.querySelector('[data-dl-proton]')?.addEventListener('click', () => navigate('proton'));
      } else if (isVerifying) {
        ctrlEl.innerHTML = `<button type="button" class="btn btn-sm" disabled>${icon('shield-check', 14)}<span>Verifying</span></button>`;
      } else if (isPaused) {
        ctrlEl.innerHTML = `
          <button type="button" class="btn btn-primary btn-sm" data-dl-resume title="Resume download">${icon('play', 14)}<span>Resume</span></button>
          <button type="button" class="btn btn-icon btn-danger" data-dl-cancel title="Cancel download" aria-label="Cancel download">${icon('x', 15)}</button>
        `;
        ctrlEl.querySelector('[data-dl-resume]')?.addEventListener('click', () => bridge.resumeDownload(job.job_id));
        ctrlEl.querySelector('[data-dl-cancel]')?.addEventListener('click', () => handleCancelDownload(job.job_id));
      } else {
        ctrlEl.innerHTML = `
          <button type="button" class="btn btn-sm" data-dl-pause title="Pause download">${icon('pause', 14)}<span>Pause</span></button>
          <button type="button" class="btn btn-icon btn-danger" data-dl-cancel title="Cancel download" aria-label="Cancel download">${icon('x', 15)}</button>
        `;
        ctrlEl.querySelector('[data-dl-pause]')?.addEventListener('click', () => bridge.pauseDownload(job.job_id));
        ctrlEl.querySelector('[data-dl-cancel]')?.addEventListener('click', () => handleCancelDownload(job.job_id));
      }
    }
  }

  const wf = activeWaveforms.get(job.job_id);
  if (wf) {
    wf.update(job.speed_samples || [], isPaused, isCompleted);
  }
}

function syncDownloadsListDom() {
  const host = document.getElementById('dl-list-host');
  if (!host) return;

  const visibleJobs = state.downloads.filter((d) => d.stage !== 'cancelled');
  if (!visibleJobs.length) {
    destroyWaveforms();
    host.innerHTML = `
      <div class="empty-state">
        <div class="empty-state-icon">${icon('arrow-down', 22)}</div>
        <h3>No active downloads</h3>
        <p>Install any game from your library or download GE-Proton to watch live wire-speed streaming extraction here.</p>
        <button type="button" class="btn btn-primary" id="btn-dl-to-lib">Browse library</button>
      </div>
    `;
    document.getElementById('btn-dl-to-lib')?.addEventListener('click', () => navigate('library'));
    return;
  }

  if (host.querySelector('.empty-state')) {
    host.innerHTML = '';
  }

  const keepIds = new Set(visibleJobs.map((j) => j.job_id));
  for (const card of [...host.querySelectorAll('.dl-card')]) {
    const jid = card.dataset.jobId;
    if (!keepIds.has(jid)) {
      activeWaveforms.get(jid)?.destroy();
      activeWaveforms.delete(jid);
      card.remove();
    }
  }

  for (const job of visibleJobs) {
    let card = host.querySelector(`.dl-card[data-job-id="${CSS.escape(job.job_id)}"]`);
    if (!card) {
      card = buildDownloadCard(job);
      host.appendChild(card);
    } else {
      patchDownloadCard(card, job);
    }
  }
}

// ---------- View 4: Proton & Compatibility ----------

function renderProtonView() {
  const latest = state.latestGeProton;
  const alreadyInstalled = latest
    ? state.protonInstallations.some((p) => p.name === latest.tag_name)
    : false;

  const latestBoxHtml = latest
    ? `
      <div class="proton-row" style="margin-top:14px;">
        <div>
          <div class="proton-row-title">${esc(latest.tag_name)}</div>
          <div class="small muted">
            ${esc(latest.tarball_name)} (${esc(humanBytes(latest.tarball_size))})${latest.sha512_url ? ', SHA-512 verified' : ''}
          </div>
        </div>
        <div>
          ${alreadyInstalled
            ? `<span class="badge badge-accent">${icon('check', 13)}Installed</span>`
            : `<button type="button" class="btn btn-primary btn-sm" id="btn-install-ge-proton">${icon('arrow-down', 14)}<span>Download and install</span></button>`}
        </div>
      </div>
    `
    : state.geProtonError
      ? `<p class="form-error">${esc(state.geProtonError)}</p>`
      : '';

  const rowsHtml = state.protonInstallations.length
    ? state.protonInstallations.map((p) => `
        <div class="proton-row">
          <div>
            <div class="proton-row-title">${esc(p.name)}</div>
            <div class="proton-row-path">${esc(p.root_dir)}</div>
          </div>
          <span class="badge ${p.source === 'OpenSyncManaged' ? 'badge-accent' : ''}">${esc(protonSourceLabel(p.source))}</span>
        </div>
      `).join('')
    : `
      <div class="empty-state">
        <div class="empty-state-icon">${icon('cpu', 22)}</div>
        <h3>No Proton builds detected</h3>
        <p>Check for the latest GE-Proton release above to run Windows games with automatic per-game WINE prefix isolation.</p>
      </div>
    `;

  viewEl.innerHTML = `
    <div class="page-header-row">
      <h1 class="page-title">proton</h1>
      <button type="button" class="btn btn-sm" id="btn-rescan-proton">${icon('refresh-cw', 14)}<span>Rescan runtimes</span></button>
    </div>

    <section class="panel">
      <div class="proton-hero">
        <div>
          <h2>Check and install latest GE-Proton</h2>
          <p class="small muted">Fetches the latest GloriousEggroll GE-Proton release from GitHub, verifies its SHA-512 checksum, and installs it into OpenSync's managed runtime folder.</p>
        </div>
        <button type="button" class="btn btn-primary" id="btn-check-ge-proton" ${state.checkingGeProton ? 'disabled' : ''}>
          ${icon('refresh-cw', 15, state.checkingGeProton ? 'spin' : '')}
          <span>${state.checkingGeProton ? 'Checking GitHub…' : 'Check latest release'}</span>
        </button>
      </div>
      ${latestBoxHtml}
    </section>

    <div class="section-title">Detected runtimes (${state.protonInstallations.length})</div>
    <div class="proton-list">${rowsHtml}</div>
  `;

  document.getElementById('btn-rescan-proton')?.addEventListener('click', async () => {
    try {
      state.protonInstallations = await bridge.listProtonInstallations();
      renderProtonView();
      showToast(`Found ${state.protonInstallations.length} Proton runtimes`, 'accent', 'cpu');
    } catch (err) {
      showToast(String(err?.message || err), 'danger', 'alert-triangle');
    }
  });

  document.getElementById('btn-check-ge-proton')?.addEventListener('click', async () => {
    state.checkingGeProton = true;
    state.geProtonError = null;
    renderProtonView();
    try {
      state.latestGeProton = await bridge.fetchLatestGeProton();
    } catch (err) {
      state.geProtonError = String(err?.message || err);
    } finally {
      state.checkingGeProton = false;
      renderProtonView();
    }
  });

  document.getElementById('btn-install-ge-proton')?.addEventListener('click', async () => {
    if (!state.latestGeProton) return;
    try {
      await bridge.downloadGeProton(state.latestGeProton);
      await refreshDownloads();
      showToast(`Downloading ${state.latestGeProton.tag_name}`, 'accent', 'arrow-down');
      navigate('downloads');
    } catch (err) {
      showToast(String(err?.message || err), 'danger', 'alert-triangle');
    }
  });
}

// ---------- View 5: Profiles / Instances ----------

function renderProfilesView() {
  const activeId = state.activeProfile?.id;
  const listHtml = state.profiles.length
    ? state.profiles.map((p) => {
        const isActive = p.id === activeId;
        return `
          <div class="profile-card ${isActive ? 'active' : ''}">
            <div>
              <div class="dl-title-row">
                <span class="profile-menu-name">${esc(p.label)}</span>
                ${isActive ? '<span class="badge badge-accent">Active</span>' : ''}
                ${p.role === 'admin' ? '<span class="badge badge-warn">Admin</span>' : ''}
              </div>
              <div class="small muted">@${esc(p.username)} on ${esc(p.server_url)}</div>
            </div>
            <div class="profile-card-actions">
              ${!isActive ? `<button type="button" class="btn btn-sm btn-primary" data-prof-switch="${esc(p.id)}">Switch</button>` : ''}
              <button type="button" class="btn btn-sm btn-danger" data-prof-delete="${esc(p.id)}" title="Remove saved instance" aria-label="Remove saved instance">${icon('trash-2', 14)}</button>
            </div>
          </div>
        `;
      }).join('')
    : `
      <div class="empty-state">
        <div class="empty-state-icon">${icon('server', 22)}</div>
        <h3>No saved instances</h3>
        <p>Connect to your OpenSync LAN server using the form on the right.</p>
      </div>
    `;

  viewEl.innerHTML = `
    <div class="page-header-row">
      <h1 class="page-title">instances</h1>
    </div>

    <div class="profiles-layout">
      <section class="panel">
        <h2>Saved profiles</h2>
        <p class="small muted" style="margin-bottom:14px;">Switch between OpenSync servers in one click. Installed games, playtime, and offline sync queues stay cleanly scoped per profile.</p>
        ${listHtml}
      </section>

      <section class="panel" id="inline-connect-panel"></section>
    </div>
  `;

  mountConnectForm(document.getElementById('inline-connect-panel'), {
    onSuccess: async () => {
      await refreshAll();
      navigate('library');
    },
  });

  viewEl.querySelectorAll('[data-prof-switch]').forEach((btn) => {
    btn.addEventListener('click', async () => {
      try {
        await bridge.switchProfile(btn.dataset.profSwitch);
        await refreshAll();
        showToast('Switched OpenSync instance', 'accent', 'check');
      } catch (err) {
        showToast(String(err?.message || err), 'danger', 'alert-triangle');
      }
    });
  });

  viewEl.querySelectorAll('[data-prof-delete]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const pid = btn.dataset.profDelete;
      const prof = state.profiles.find((x) => x.id === pid);
      openConfirmModal({
        title: `Remove ${prof?.label || 'profile'}`,
        body: 'Removes this saved server profile and its stored session cookie from this device. Local game folders on disk are kept intact.',
        confirmLabel: 'Remove profile',
        danger: true,
        onConfirm: async () => {
          try {
            await bridge.deleteProfile(pid);
            await refreshAll();
            showToast('Profile removed', 'accent', 'trash-2');
          } catch (err) {
            showToast(String(err?.message || err), 'danger', 'alert-triangle');
          }
        },
      });
    });
  });
}

function mountConnectForm(container, { onSuccess, onCancel = null, fullWidthSubmit = false }) {
  let mode = 'login'; // 'login' | 'register'
  let showPw = false;

  const render = () => {
    const prevUrl = container.querySelector('#cf-url')?.value || '';
    const prevLabel = container.querySelector('#cf-label')?.value || '';
    const prevUser = container.querySelector('#cf-user')?.value || '';

    container.innerHTML = `
      <div class="auth-tabs">
        <button type="button" class="${mode === 'login' ? 'active' : ''}" data-auth-tab="login">Sign in</button>
        <button type="button" class="${mode === 'register' ? 'active' : ''}" data-auth-tab="register">Create account</button>
      </div>
      <form id="connect-profile-form">
        <div class="field">
          <label for="cf-url">Server URL</label>
          <input id="cf-url" class="input" type="text" placeholder="http://127.0.0.1:3000" value="${esc(prevUrl)}" required />
        </div>
        <div class="field">
          <label for="cf-label">Instance label</label>
          <input id="cf-label" class="input" type="text" placeholder="Home LAN" value="${esc(prevLabel)}" />
        </div>
        <div class="field">
          <label for="cf-user">Username</label>
          <input id="cf-user" class="input" type="text" placeholder="your handle" value="${esc(prevUser)}" required />
        </div>
        <div class="field">
          <label for="cf-pass">Password</label>
          <div class="input-pw-wrap">
            <input id="cf-pass" class="input" type="${showPw ? 'text' : 'password'}" placeholder="Enter password" required />
            <button type="button" class="pw-toggle" id="cf-pw-toggle" aria-label="${showPw ? 'Hide password' : 'Show password'}" title="${showPw ? 'Hide password' : 'Show password'}">
              ${icon(showPw ? 'eye-off' : 'eye', 16)}
            </button>
          </div>
        </div>
        <div class="form-error" id="cf-err"></div>
        <div class="modal-actions">
          ${onCancel ? '<button type="button" class="btn btn-ghost" id="cf-cancel">Cancel</button>' : ''}
          <button type="submit" class="btn btn-primary${fullWidthSubmit && !onCancel ? ' btn-full' : ''}" id="cf-submit">
            ${mode === 'register' ? 'Create account and connect' : 'Sign in and connect'}
          </button>
        </div>
      </form>
    `;

    container.querySelectorAll('[data-auth-tab]').forEach((t) => {
      t.addEventListener('click', () => {
        mode = t.dataset.authTab;
        render();
      });
    });

    container.querySelector('#cf-pw-toggle')?.addEventListener('click', () => {
      const pwInput = container.querySelector('#cf-pass');
      showPw = !showPw;
      if (pwInput) {
        pwInput.type = showPw ? 'text' : 'password';
      }
      const btn = container.querySelector('#cf-pw-toggle');
      if (btn) {
        btn.innerHTML = icon(showPw ? 'eye-off' : 'eye', 16);
        btn.setAttribute('aria-label', showPw ? 'Hide password' : 'Show password');
        btn.title = showPw ? 'Hide password' : 'Show password';
      }
    });

    container.querySelector('#cf-cancel')?.addEventListener('click', onCancel);

    container.querySelector('#connect-profile-form')?.addEventListener('submit', async (e) => {
      e.preventDefault();
      const errEl = container.querySelector('#cf-err');
      const submitBtn = container.querySelector('#cf-submit');
      errEl.textContent = '';

      const rawUrl = container.querySelector('#cf-url').value.trim();
      const server_url = normalizeServerUrl(rawUrl);
      const label = container.querySelector('#cf-label').value.trim() || server_url.replace(/^https?:\/\//, '');
      const username = container.querySelector('#cf-user').value.trim();
      const password = container.querySelector('#cf-pass').value;

      submitBtn.disabled = true;
      try {
        await bridge.connectProfile({
          label,
          server_url,
          username,
          password,
          register: mode === 'register',
        });
        showToast(`Connected to ${label}`, 'accent', 'wifi');
        await onSuccess();
      } catch (err) {
        errEl.textContent = String(err?.message || err);
      } finally {
        submitBtn.disabled = false;
      }
    });
  };

  render();
}

// ---------- Modals & Lightbox ----------

function closeModal() {
  modalRoot.innerHTML = '';
  gamepadCtrl?.onViewRendered();
}

function openConnectModal() {
  modalRoot.innerHTML = `
    <div class="modal-backdrop" id="connect-modal-bg">
      <div class="modal-card" role="dialog" aria-modal="true" aria-label="Connect OpenSync instance">
        <div class="modal-header">
          <h2>Connect OpenSync instance</h2>
          <button type="button" class="btn btn-ghost btn-icon" id="modal-close-x" aria-label="Close">${icon('x', 16)}</button>
        </div>
        <div id="modal-connect-host"></div>
      </div>
    </div>
  `;
  document.getElementById('modal-close-x')?.addEventListener('click', closeModal);
  document.getElementById('connect-modal-bg')?.addEventListener('click', (e) => {
    if (e.target.id === 'connect-modal-bg') closeModal();
  });
  mountConnectForm(document.getElementById('modal-connect-host'), {
    onCancel: closeModal,
    onSuccess: async () => {
      closeModal();
      await refreshAll();
      navigate('library');
    },
  });
  gamepadCtrl?.onViewRendered();
}

function openConfirmModal({ title, body, confirmLabel = 'Confirm', danger = false, onConfirm }) {
  modalRoot.innerHTML = `
    <div class="modal-backdrop" id="confirm-modal-bg">
      <div class="modal-card" role="dialog" aria-modal="true" aria-label="${esc(title)}">
        <div class="modal-header">
          <h2>${esc(title)}</h2>
          <button type="button" class="btn btn-ghost btn-icon" id="confirm-close-x" aria-label="Close">${icon('x', 16)}</button>
        </div>
        <p class="muted" style="margin:0;">${esc(body)}</p>
        <div class="modal-actions">
          <button type="button" class="btn btn-ghost" id="confirm-cancel">Cancel</button>
          <button type="button" class="btn ${danger ? 'btn-danger' : 'btn-primary'}" id="confirm-ok">${esc(confirmLabel)}</button>
        </div>
      </div>
    </div>
  `;
  document.getElementById('confirm-close-x')?.addEventListener('click', closeModal);
  document.getElementById('confirm-cancel')?.addEventListener('click', closeModal);
  document.getElementById('confirm-modal-bg')?.addEventListener('click', (e) => {
    if (e.target.id === 'confirm-modal-bg') closeModal();
  });
  document.getElementById('confirm-ok')?.addEventListener('click', async () => {
    closeModal();
    await onConfirm();
  });
  gamepadCtrl?.onViewRendered();
}

function openLightbox(src, alt) {
  lightboxRoot.innerHTML = `
    <div class="modal-backdrop" id="lightbox-bg">
      <div class="lightbox-frame">
        <img src="${src}" alt="${esc(alt)}" />
        <button type="button" class="btn btn-icon lightbox-close" id="lightbox-close-btn" aria-label="Close lightbox">${icon('x', 16)}</button>
      </div>
    </div>
  `;
  const close = () => {
    lightboxRoot.innerHTML = '';
    gamepadCtrl?.onViewRendered();
  };
  document.getElementById('lightbox-close-btn')?.addEventListener('click', close);
  document.getElementById('lightbox-bg')?.addEventListener('click', (e) => {
    if (e.target.id === 'lightbox-bg') close();
  });
  gamepadCtrl?.onViewRendered();
}

// ---------- Core Action Handlers ----------

async function handleLaunchGame(game) {
  if (state.playingGameIds.has(game.game_id)) return;
  try {
    state.playingGameIds.add(game.game_id);
    renderCurrentView();
    await bridge.launchGame(game.game_id);
    showToast(`Launching ${game.name}`, 'accent', 'play');
  } catch (err) {
    state.playingGameIds.delete(game.game_id);
    renderCurrentView();
    showToast(String(err?.message || err), 'danger', 'alert-triangle');
  }
}

async function handleStartDownload(game, fresh = false) {
  try {
    await bridge.startGameDownload(game.game_id, fresh);
    await refreshDownloads();
    updateNavState();
    showToast(`Downloading ${game.name}`, 'accent', 'arrow-down');
    if (state.view === 'library') renderLibraryGrid();
    else if (state.view === 'game-detail') renderGameDetailView();
  } catch (err) {
    showToast(String(err?.message || err), 'danger', 'alert-triangle');
  }
}

async function handleCancelDownload(jobId) {
  try {
    await bridge.cancelDownload(jobId);
    state.downloads = state.downloads.filter((d) => d.job_id !== jobId);
    updateNavState();
    if (state.view === 'downloads') syncDownloadsListDom();
    else if (state.view === 'library') renderLibraryGrid();
    showToast('Download cancelled', 'warn', 'x');
  } catch (err) {
    showToast(String(err?.message || err), 'danger', 'alert-triangle');
  }
}

async function handleManualSync(game, force = false) {
  state.syncingGameIds.add(game.game_id);
  renderCurrentView();
  try {
    const report = await bridge.syncGameNow(game.game_id, force);
    state.syncingGameIds.delete(game.game_id);
    await refreshAll();
    const count = (report?.uploaded_files || 0) + (report?.deleted_paths || 0);
    showToast(
      count > 0 ? `Synced ${report.uploaded_files} files for ${game.name}` : `${game.name} saves are already up to date`,
      'accent',
      'check',
    );
  } catch (err) {
    state.syncingGameIds.delete(game.game_id);
    renderCurrentView();
    const msg = String(err?.message || err);
    if (!force && msg.toLowerCase().includes('mass deletion')) {
      openConfirmModal({
        title: 'Confirm large save deletion',
        body: `${msg}. Proceed with force sync?`,
        confirmLabel: 'Force sync',
        danger: true,
        onConfirm: () => handleManualSync(game, true),
      });
      return;
    }
    showToast(msg, 'danger', 'alert-triangle');
  }
}

async function quickInstallLatestGeProton() {
  try {
    showToast('Checking latest GE-Proton release…', 'accent', 'refresh-cw');
    const rel = await bridge.fetchLatestGeProton();
    state.latestGeProton = rel;
    await bridge.downloadGeProton(rel);
    await refreshDownloads();
    showToast(`Downloading ${rel.tag_name}`, 'accent', 'arrow-down');
    navigate('downloads');
  } catch (err) {
    showToast(`GE-Proton fetch failed: ${err?.message || err}`, 'danger', 'alert-triangle');
  }
}

// ---------- Data Sync & Live Events ----------

async function refreshDownloads() {
  try {
    const raw = await bridge.listDownloads();
    state.downloads = (raw || []).map(normalizeDownloadJob).filter(Boolean);
    updateNavState();
    if (state.view === 'downloads') syncDownloadsListDom();
  } catch {
    // ignore transient error
  }
}

async function refreshLibrary() {
  if (!state.activeProfile) {
    state.library = [];
    return;
  }
  try {
    state.library = await bridge.listLibrary();
  } catch {
    // keep cached library when offline
  }
}

async function refreshAll() {
  const boot = await bridge.getBootstrapState();
  state.activeProfile = boot.active_profile || null;
  state.profiles = boot.profiles || [];
  state.connectionState = normalizeConnectionState(boot.connection_state);
  state.protonInstallations = boot.proton_installations || [];
  state.pendingSyncs = boot.pending_syncs || [];
  state.downloads = (boot.downloads || []).map(normalizeDownloadJob).filter(Boolean);
  state.bandwidthLimitBps = Number(boot.bandwidth_limit_bps || 0);
  await refreshLibrary();
  updateNavState();
  renderStatusStrip();
  renderCurrentView();
}

function upsertDownloadInState(job) {
  if (!job || !job.job_id) return null;
  const idx = state.downloads.findIndex((d) => d.job_id === job.job_id);
  const prevStage = idx !== -1 ? state.downloads[idx].stage : null;
  if (job.stage === 'cancelled') {
    if (idx !== -1) state.downloads.splice(idx, 1);
  } else if (idx !== -1) {
    state.downloads[idx] = job;
  } else {
    state.downloads.unshift(job);
  }
  return prevStage;
}

function handleLauncherEvent(evt) {
  if (!evt || typeof evt !== 'object') return;
  const type = String(evt.type || '').toLowerCase();

  switch (type) {
    case 'connection_changed':
    case 'connectionchanged': {
      state.connectionState = normalizeConnectionState(evt.state);
      renderStatusStrip();
      break;
    }
    case 'download_updated':
    case 'downloadupdated': {
      const job = normalizeDownloadJob(evt.job);
      if (!job) break;
      const prevStage = upsertDownloadInState(job);
      updateNavState();

      if (job.stage === 'completed' && prevStage !== 'completed') {
        showToast(`${job.title} is ready`, 'accent', 'check');
        refreshLibrary().then(() => {
          if (state.view === 'downloads') syncDownloadsListDom();
          else renderCurrentView();
        });
        break;
      }

      if (job.stage === 'failed' && prevStage !== 'failed') {
        showToast(`Download failed: ${job.error || 'Unknown error'}`, 'danger', 'alert-triangle');
      }

      if (state.view === 'downloads') {
        syncDownloadsListDom();
      } else if (state.view === 'library' && job.kind === 'game' && job.game_id != null) {
        const card = document.querySelector(`.card[data-game-id="${job.game_id}"]`);
        if (card) {
          const pct = job.total_bytes > 0
            ? Math.min(100, Math.round((job.bytes_downloaded / job.total_bytes) * 100))
            : 0;
          const pctEl = card.querySelector('.card-dl-pct');
          if (pctEl) pctEl.textContent = `${pct}%`;
          const pill = card.querySelector('.card-top-pill.downloading');
          if (pill) pill.textContent = job.paused ? 'Paused' : `${pct}%`;
        }
      }
      break;
    }
    case 'game_session_started':
    case 'gamesessionstarted': {
      state.playingGameIds.add(evt.game_id);
      renderCurrentView();
      break;
    }
    case 'sync_started':
    case 'syncstarted': {
      state.syncingGameIds.add(evt.game_id);
      const g = state.library.find((x) => x.game_id === evt.game_id);
      showToast(`Syncing saves for ${g ? g.name : 'game'}…`, 'accent', 'refresh-cw');
      if (state.view === 'library') renderLibraryGrid();
      else if (state.view === 'game-detail') renderGameDetailView();
      break;
    }
    case 'game_session_ended':
    case 'gamesessionended': {
      state.playingGameIds.delete(evt.game_id);
      state.syncingGameIds.delete(evt.game_id);
      const g = state.library.find((x) => x.game_id === evt.game_id);
      const gName = g ? g.name : 'game';
      if (evt.synced) {
        showToast(`Saves synced for ${gName}`, 'accent', 'check');
      } else if (evt.error) {
        showToast(`Save sync queued for ${gName}: ${evt.error}`, 'warn', 'wifi-off');
      }
      refreshAll();
      break;
    }
    case 'proton_installed':
    case 'protoninstalled': {
      if (evt.installation) {
        const exists = state.protonInstallations.some((p) => p.name === evt.installation.name);
        if (!exists) state.protonInstallations.unshift(evt.installation);
        showToast(`${evt.installation.name} installed`, 'accent', 'cpu');
      }
      if (state.view === 'proton' || state.view === 'game-detail') {
        renderCurrentView();
      }
      break;
    }
    default:
      break;
  }
}

// ---------- Controller / Gamepad Wiring ----------

function initGamepad() {
  gamepadCtrl = createGamepadController({
    getView: () => state.view,
    onCardPrimary: (gameId) => {
      const g = state.library.find((x) => x.game_id === gameId);
      if (g) runCardPrimaryAction(g);
    },
    onCardDetails: (gameId) => {
      navigate('game-detail', { gameId });
    },
    onSecondaryAction: () => {
      if (state.view === 'game-detail') {
        const syncBtn = document.getElementById('btn-detail-sync');
        if (syncBtn && !syncBtn.disabled) {
          syncBtn.click();
          return;
        }
        document.getElementById('btn-detail-fresh')?.click();
      }
    },
    onBack: () => {
      if (lightboxRoot.innerHTML) {
        lightboxRoot.innerHTML = '';
        gamepadCtrl?.onViewRendered();
        return;
      }
      if (modalRoot.innerHTML) {
        closeModal();
        return;
      }
      if (state.profileMenuOpen) {
        state.profileMenuOpen = false;
        renderStatusStrip();
        gamepadCtrl?.onViewRendered();
        return;
      }
      if (state.view === 'game-detail') {
        navigate('library');
        return;
      }
      if (document.activeElement?.tagName === 'INPUT') {
        document.activeElement.blur();
      }
    },
    onYAction: () => {
      if (state.view === 'library') {
        const search = document.getElementById('lib-search');
        if (search) {
          search.focus();
          search.select();
        }
      } else if (state.view === 'proton') {
        document.getElementById('btn-rescan-proton')?.click();
      }
    },
    onCycleTab: (delta) => {
      if (modalRoot.innerHTML || lightboxRoot.innerHTML) return;
      const curTab = state.view === 'game-detail' ? 'library' : state.view;
      const idx = Math.max(0, TAB_ORDER.indexOf(curTab));
      const next = TAB_ORDER[(idx + delta + TAB_ORDER.length) % TAB_ORDER.length];
      navigate(next);
    },
    onToggleProfileMenu: () => {
      state.profileMenuOpen = !state.profileMenuOpen;
      renderStatusStrip();
      gamepadCtrl?.onViewRendered();
    },
    onGamepadConnected: () => {
      showToast('Controller connected', 'accent', 'gamepad-2');
    },
  });
}

// ---------- Boot ----------

async function boot() {
  initTopbarNav();
  initGamepad();
  await listenEvents(handleLauncherEvent);
  await refreshAll();
}

boot();
