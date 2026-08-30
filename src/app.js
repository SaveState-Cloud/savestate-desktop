// ─────────────────────────────────────────────────────────────────────
// SaveState Vault — Frontend Logic (V2)
// ─────────────────────────────────────────────────────────────────────

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { open, confirm: confirmDialog } = window.__TAURI__.dialog;
const vaultRecoveryUi = window.SaveStateVaultRecovery;
const storageUsageUi = window.SaveStateStorageUsage;

// ── Auto-updater state ───────────────────────────────────────────
let availableUpdateVersion = null;
let dismissedUpdateVersion = null;
let currentAppVersion = null;
let updateCheckPromise = null;
let updateInstallInProgress = false;
let updatePhase = 'idle';
let updateStatusMessage = 'Update status is loading…';
let updateProgressPercent = 0;
const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;

// ── Page map ──────────────────────────────────────────────────────
const pages = {
    dashboard: document.getElementById('page-dashboard'),
    profiles:  document.getElementById('page-profiles'),
    databases: document.getElementById('page-databases'),
    backup:    document.getElementById('page-backup'),
    backups:   document.getElementById('page-backups'),
    settings:  document.getElementById('page-settings'),
};

// ── UI State ──────────────────────────────────────────────────────
let currentAccount = null;
let restoreTarget = null;        // { key, filename }
let restoreInProgress = false;
let restoreCancelRequested = false;
let explorerTarget = null;       // { key, filename }
let explorerManifest = null;     // manifest JSON
let selectedExplorerFiles = new Set();
let currentFolder = '/';       // Current folder path in backups view
let folderList = [];           // Available folders for move/profile dropdowns
let storageCleanupPending = false;
const activeToasts = new Map();
let globalProgressHideTimer = null;
let pendingBackupErrorToast = null;
let pendingRestoreErrorToast = null;
let repositoryRecoveryPromptOpen = false;
let authenticatedSessionActive = false;
let repositoryWarmupPromise = null;
let repositorySessionGeneration = 0;
let legacyProfileNoticeShown = false;
let pendingVaultLoginResult = null;
let discordWebhookConfigured = false;
let databaseTools = [];
let databaseProfiles = [];
let databaseConnectionResult = null;
let databaseConnectionFingerprint = null;
let databaseSelectedDatabases = new Set();
let databaseSelectedTables = new Set();
let pendingProfileDeletion = null;
let organizationEnrollmentPreview = null;
let organizationAvailableInstallations = [];
let selectedOrganizationInstallationId = null;
let accountWorkspaces = [];
let workspaceSwitchInProgress = false;
let workspaceUiGeneration = 0;

// ────────────────────────────────────────────────────────────────
// Initialization
// ────────────────────────────────────────────────────────────────
async function init() {
    setupEventListeners();
    setupTauriListeners();

    // Start both local version lookup and the network update check immediately.
    // Neither should delay remembered sign-in or repository warm-up.
    renderUpdaterUi();
    void loadCurrentAppVersion();
    void checkForUpdates({ revealAvailable: true });

    await checkAuthStatus();
    setInterval(() => void checkForUpdates(), UPDATE_CHECK_INTERVAL_MS);
}

document.addEventListener('DOMContentLoaded', init);

// ────────────────────────────────────────────────────────────────
// Event Listeners
// ────────────────────────────────────────────────────────────────
function setupEventListeners() {
    // Re-warm after the app is reopened from the tray. The native cache makes
    // this a no-op while the existing 15-minute repository session is valid.
    window.addEventListener('focus', () => {
        warmRepositoryInBackground();
    });

    // Nav links
    document.querySelectorAll('.nav-link').forEach(link => {
        link.addEventListener('click', (e) => {
            e.preventDefault();
            navigateTo(link.getAttribute('data-view'));
        });
    });

    // Dashboard quick-action buttons
    document.getElementById('btn-goto-profiles').addEventListener('click', () => navigateTo('profiles'));
    document.getElementById('btn-goto-backup').addEventListener('click', () => navigateTo('backup'));
    document.getElementById('btn-goto-backups').addEventListener('click', () => navigateTo('backups'));

    // Resume subscription
    document.getElementById('btn-resume-sub').addEventListener('click', async () => {
        try {
            await invoke('cmd_resume_subscription');
            showToast('Subscription resumed!', 'success');
            loadDashboard();
        } catch (err) {
            showToast('Failed to resume: ' + String(err), 'error');
        }
    });

    // Auto-updater banner buttons
    document.getElementById('btn-install-update').addEventListener('click', () => installUpdate());
    document.getElementById('btn-settings-install-update').addEventListener('click', () => installUpdate());
    document.getElementById('btn-check-updates').addEventListener('click', () => {
        void checkForUpdates({ revealAvailable: true });
    });
    document.getElementById('btn-dismiss-update').addEventListener('click', () => {
        dismissedUpdateVersion = availableUpdateVersion;
        document.getElementById('update-banner').classList.add('hidden');
    });

    // Login
    document.getElementById('login-form').addEventListener('submit', async (e) => {
        e.preventDefault();
        const email = document.getElementById('login-email').value.trim();
        const password = document.getElementById('login-password').value;
        const rememberMe = document.getElementById('login-remember').checked;
        const btn = document.getElementById('login-btn');
        const errorEl = document.getElementById('login-error');

        btn.querySelector('.btn-text').textContent = 'Signing in…';
        btn.querySelector('.spinner').classList.remove('hidden');
        btn.disabled = true;
        errorEl.classList.add('hidden');

        try {
            const result = await invoke('cmd_login', { email, password, rememberMe });
            document.getElementById('login-password').value = '';
            await handleVaultLoginResult(result);
        } catch (err) {
            errorEl.textContent = friendlyError(err);
            errorEl.classList.remove('hidden');
        } finally {
            btn.querySelector('.btn-text').textContent = 'Sign In';
            btn.querySelector('.spinner').classList.add('hidden');
            btn.disabled = false;
        }
    });

    document.getElementById('btn-copy-vault-recovery-key').addEventListener('click', async () => {
        const recoveryKey = document.getElementById('vault-recovery-key').textContent.trim();
        if (!recoveryKey) return;
        try {
            await navigator.clipboard.writeText(recoveryKey);
            showToast('Vault recovery key copied. Store it offline.', 'success');
        } catch {
            showToast('Could not copy automatically. Select the vault recovery key and copy it manually.', 'error');
        }
    });

    document.getElementById('btn-confirm-vault-recovery-key').addEventListener('click', async () => {
        const button = document.getElementById('btn-confirm-vault-recovery-key');
        const errorEl = document.getElementById('vault-setup-error');
        const accountPassword = document.getElementById('vault-setup-account-password').value;
        const acknowledged = document.getElementById('vault-recovery-ack').checked;
        errorEl.classList.add('hidden');
        if (!accountPassword) {
            errorEl.textContent = 'Enter your current account password.';
            errorEl.classList.remove('hidden');
            return;
        }
        button.disabled = true;
        try {
            const result = await invoke('cmd_confirm_vault_recovery_key', { accountPassword, acknowledged });
            clearVaultFlowSecrets();
            await handleVaultLoginResult(result);
        } catch (error) {
            errorEl.textContent = friendlyError(error);
            errorEl.classList.remove('hidden');
        } finally {
            button.disabled = false;
        }
    });

    document.getElementById('vault-unlock-method').addEventListener('change', updateVaultUnlockLabel);

    document.getElementById('btn-unlock-vault').addEventListener('click', async () => {
        const button = document.getElementById('btn-unlock-vault');
        const errorEl = document.getElementById('vault-unlock-error');
        const method = document.getElementById('vault-unlock-method').value;
        const secret = document.getElementById('vault-unlock-secret').value;
        const accountPassword = document.getElementById('vault-current-account-password').value;
        errorEl.classList.add('hidden');
        if (!secret || !accountPassword) {
            errorEl.textContent = 'Enter both the vault unlock factor and your current account password.';
            errorEl.classList.remove('hidden');
            return;
        }
        button.disabled = true;
        try {
            const result = await invoke('cmd_unlock_vault', { method, secret, accountPassword });
            clearVaultFlowSecrets();
            await handleVaultLoginResult(result);
        } catch (error) {
            errorEl.textContent = friendlyError(error);
            errorEl.classList.remove('hidden');
        } finally {
            button.disabled = false;
        }
    });

    document.getElementById('btn-abandon-vault-unlock').addEventListener('click', async () => {
        await invoke('cmd_abandon_vault_unlock');
        clearVaultFlowSecrets();
        pendingVaultLoginResult = null;
        showLoginAuthCard();
    });

    // Logou
    document.getElementById('btn-logout').addEventListener('click', async () => {
        const button = document.getElementById('btn-logout');
        button.disabled = true;
        let preparedLogout = null;
        try {
            preparedLogout = await invoke('cmd_prepare_logout');
            const activeBackups = preparedLogout.activeBackups || [];
            const confirmed = await window.SaveStateLogout.confirmActiveBackups(
                activeBackups,
                confirmDialog,
            );
            if (!confirmed) {
                await invoke('cmd_abort_logout', { logoutToken: preparedLogout.token });
                preparedLogout = null;
                return;
            }
            button.textContent = activeBackups.length > 0 ? 'Stopping backups…' : 'Signing out…';
            const result = await invoke('cmd_logout', { logoutToken: preparedLogout.token });
            preparedLogout = null;
            endAuthenticatedSession();
            pendingVaultLoginResult = null;
            clearVaultFlowSecrets();
            showLoginAuthCard();
            resetBackupMode();
            hideGlobalProgress();
            showView('login');
            if (Number(result?.cancelledBackups) > 0) {
                showToast('Active backups were stopped safely.', 'info');
            }
        } catch (error) {
            if (preparedLogout) {
                await invoke('cmd_abort_logout', { logoutToken: preparedLogout.token }).catch(() => {});
            }
            showToast('Could not sign out: ' + friendlyError(error), 'error');
        } finally {
            button.disabled = false;
            button.textContent = 'Sign Out';
        }
    });

    // ── Quick Backup ──────────────────────────────────────────
    document.getElementById('btn-pick-files').addEventListener('click', async () => {
        const files = await open({ multiple: true });
        if (files && files.length > 0) {
            startBackupMode();
            try {
                const folder = document.getElementById('quick-backup-folder')?.value || '/';
                await invoke('cmd_backup_files', { paths: Array.isArray(files) ? files : [files], folder });
            } catch (err) {
                failBackupUi(err);
            }
        }
    });

    document.getElementById('btn-pick-folder').addEventListener('click', async () => {
        const folder = await open({ directory: true });
        if (folder) {
            startBackupMode();
            try {
                const destFolder = document.getElementById('quick-backup-folder')?.value || '/';
                await invoke('cmd_backup_folder', { path: folder, folder: destFolder });
            } catch (err) {
                failBackupUi(err);
            }
        }
    });

    // ── Backups ───────────────────────────────────────────────
    document.getElementById('btn-refresh-backups').addEventListener('click', loadBackups);

    // ── Restore Modal ─────────────────────────────────────────
    document.getElementById('btn-cancel-restore').addEventListener('click', async () => {
        if (!restoreInProgress || !restoreTarget) {
            resetRestoreModal();
            return;
        }
        if (restoreCancelRequested) return;

        restoreCancelRequested = true;
        const cancelBtn = document.getElementById('btn-cancel-restore');
        cancelBtn.textContent = 'Stopping…';
        cancelBtn.disabled = true;
        document.getElementById('restore-progress-msg').textContent = 'Stopping restore and removing partial files…';
        try {
            await invoke('cmd_cancel_restore', { key: restoreTarget.key });
        } catch (err) {
            restoreCancelRequested = false;
            cancelBtn.textContent = 'Stop Restore';
            cancelBtn.disabled = false;
            showToast('Could not stop restore: ' + String(err), 'error');
        }
    });

    document.getElementById('btn-pick-restore-dest').addEventListener('click', async () => {
        const folder = await open({ directory: true });
        if (folder) {
            document.getElementById('restore-dest').value = folder;
        }
    });

    document.getElementById('btn-confirm-restore').addEventListener('click', async () => {
        if (!restoreTarget) return;
        const dest = document.getElementById('restore-dest').value;
        if (!dest) { showToast('Please select a destination folder', 'error'); return; }

        try {
            restoreInProgress = true;
            restoreCancelRequested = false;
            document.getElementById('btn-confirm-restore').disabled = true;
            document.getElementById('btn-pick-restore-dest').disabled = true;
            document.getElementById('btn-cancel-restore').textContent = 'Stop Restore';
            document.getElementById('restore-progress-wrap').classList.remove('hidden');
            document.getElementById('restore-progress-msg').classList.remove('hidden');
            await invoke('cmd_restore_backup', {
                key: restoreTarget.key,
                filename: restoreTarget.filename,
                destination: dest,
            });
        } catch (err) {
            if (!String(err).toLowerCase().includes('cancelled')) {
                if (pendingRestoreErrorToast) {
                    clearTimeout(pendingRestoreErrorToast);
                    pendingRestoreErrorToast = null;
                }
                handleRepositoryError(err);
            }
            resetRestoreModal();
        }
    });

    // ── File Explorer Modal ───────────────────────────────────
    document.getElementById('btn-close-explorer').addEventListener('click', closeFileExplorer);
    document.getElementById('btn-close-explorer-bottom').addEventListener('click', closeFileExplorer);

    document.getElementById('btn-select-all-files').addEventListener('click', () => {
        const checkboxes = document.querySelectorAll('#file-tree-container input[type="checkbox"]');
        const allChecked = [...checkboxes].every(cb => cb.checked);
        checkboxes.forEach(cb => { cb.checked = !allChecked; cb.dispatchEvent(new Event('change')); });
        updateSelectedCount();
    });

    document.getElementById('btn-restore-selected').addEventListener('click', () => {
        if (selectedExplorerFiles.size === 0) return;
        document.getElementById('file-explorer-modal').classList.add('hidden');
        document.getElementById('selective-restore-count').textContent = `Restoring ${selectedExplorerFiles.size} file(s)`;
        document.getElementById('selective-restore-modal').classList.remove('hidden');
    });

    document.getElementById('btn-restore-all-explorer').addEventListener('click', () => {
        if (!explorerTarget) return;
        document.getElementById('file-explorer-modal').classList.add('hidden');
        restoreTarget = explorerTarget;
        document.getElementById('restore-dest').value = '';
        document.getElementById('restore-modal').classList.remove('hidden');
    });

    // ── Selective Restore Modal ────────────────────────────────
    document.getElementById('btn-cancel-selective').addEventListener('click', () => {
        document.getElementById('selective-restore-modal').classList.add('hidden');
    });

    document.getElementById('btn-pick-selective-dest').addEventListener('click', async () => {
        const folder = await open({ directory: true });
        if (folder) document.getElementById('selective-restore-dest').value = folder;
    });

    document.getElementById('btn-confirm-selective').addEventListener('click', async () => {
        if (!explorerTarget || selectedExplorerFiles.size === 0) return;
        const dest = document.getElementById('selective-restore-dest').value;
        if (!dest) { showToast('Please select a destination folder', 'error'); return; }

        try {
            document.getElementById('selective-progress-wrap').classList.remove('hidden');
            document.getElementById('selective-progress-msg').classList.remove('hidden');
            await invoke('cmd_restore_selected_files', {
                key: explorerTarget.key,
                filename: explorerTarget.filename,
                destination: dest,
                selectedPaths: [...selectedExplorerFiles],
            });
        } catch (err) {
            handleRepositoryError(err);
            document.getElementById('selective-progress-wrap').classList.add('hidden');
            document.getElementById('selective-progress-msg').classList.add('hidden');
            document.getElementById('selective-restore-modal').classList.add('hidden');
        }
    });

    // ── Profiles ──────────────────────────────────────────────
    document.getElementById('btn-create-profile').addEventListener('click', () => {
        openProfileModal();
    });

    document.getElementById('btn-cancel-profile').addEventListener('click', () => {
        document.getElementById('profile-modal').classList.add('hidden');
    });

    document.getElementById('btn-pick-profile-source').addEventListener('click', async () => {
        const folder = await open({ directory: true });
        if (folder) document.getElementById('profile-source').value = folder;
    });

    document.getElementById('profile-name').addEventListener('input', updateProfileFolderPreview);

    document.getElementById('profile-form').addEventListener('submit', async (e) => {
        e.preventDefault();
        const editId = document.getElementById('profile-edit-id').value;
        const name = document.getElementById('profile-name').value.trim();
        const sourcePath = document.getElementById('profile-source').value;
        const timesRaw = document.getElementById('profile-schedule-times').value.trim();
        let intervalDays = parseInt(document.getElementById('profile-schedule-interval').value) || 1;
        const retention = parseInt(document.getElementById('profile-retention').value) || 0;

        if (!name || !sourcePath) { showToast('Name and source path required', 'error'); return; }

        // Validate time forma
        let schedule = null;
        if (timesRaw) {
            intervalDays = Math.max(1, Math.min(365, intervalDays));
            const times = timesRaw.split(',').map(t => t.trim()).filter(Boolean);
            const validTime = /^([01]\d|2[0-3]):([0-5]\d)$/;
            for (const t of times) {
                if (!validTime.test(t)) {
                    showToast(`Invalid time format: "${t}". Use HH:MM (24h)`, 'error');
                    return;
                }
            }
            schedule = JSON.stringify({ times, intervalDays });
        }

        try {
            if (editId) {
                await invoke('cmd_update_profile', {
                    id: editId, name, sourcePath, schedule, retention, enabled: true, folder: '/',
                });
                showToast('Profile updated', 'success');
            } else {
                await invoke('cmd_create_profile', { name, sourcePath, schedule, retention, folder: '/' });
                showToast('Profile created', 'success');
            }
            document.getElementById('profile-modal').classList.add('hidden');
            loadProfiles();
        } catch (err) {
            showToast(String(err), 'error');
        }
    });

    document.getElementById('btn-cancel-profile-delete').addEventListener('click', closeProfileDeleteModal);
    document.getElementById('btn-confirm-profile-delete').addEventListener('click', async () => {
        if (!pendingProfileDeletion) return;
        const target = pendingProfileDeletion;
        const deleteBackups = document.getElementById('profile-delete-backups').checked;
        const button = document.getElementById('btn-confirm-profile-delete');
        button.disabled = true;
        button.textContent = deleteBackups ? 'Deleting backups…' : 'Deleting profile…';
        try {
            if (target.kind === 'database') {
                await invoke('cmd_delete_database_profile', { id: target.id, deleteBackups });
            } else {
                await invoke('cmd_delete_profile', { id: target.id, deleteBackups });
            }
            closeProfileDeleteModal();
            showToast(deleteBackups
                ? 'Profile folder and its remaining backups were deleted.'
                : 'Profile deleted. Its backup folder was preserved.', 'success');
            if (target.kind === 'database') {
                void loadDatabaseProfiles();
            } else {
                void loadProfiles();
            }
        } catch (error) {
            showToast(friendlyError(error), 'error');
        } finally {
            button.disabled = false;
            button.textContent = 'Delete Profile';
        }
    });

    // ── Database Backups ───────────────────────────────────────
    document.getElementById('btn-create-database').addEventListener('click', () => {
        void openDatabaseSetup();
    });
    document.getElementById('btn-close-database-setup').addEventListener('click', closeDatabaseSetup);
    document.getElementById('btn-cancel-database').addEventListener('click', closeDatabaseSetup);
    document.getElementById('btn-refresh-database-tools').addEventListener('click', () => {
        void loadDatabaseTools({ force: true });
    });
    document.getElementById('database-tool-bundle').addEventListener('change', applySelectedDatabaseTool);
    document.getElementById('btn-test-database').addEventListener('click', () => {
        void testDatabaseConnection();
    });
    document.querySelectorAll('input[name="database-scope"]').forEach((input) => {
        input.addEventListener('change', renderDatabaseScope);
    });
    document.getElementById('database-table-database').addEventListener('change', () => {
        databaseSelectedTables = new Set();
        document.getElementById('database-table-checklist').innerHTML = '<p class="text-muted">Load tables for the selected database.</p>';
    });
    document.getElementById('btn-load-database-tables').addEventListener('click', () => {
        void loadDatabaseTables();
    });
    document.getElementById('database-form').addEventListener('submit', (event) => {
        event.preventDefault();
        void saveDatabaseProfile();
    });
    ['database-connection-url', 'database-password', 'database-dump-executable', 'database-client-executable']
        .forEach((id) => {
            document.getElementById(id).addEventListener('input', invalidateDatabaseConnectionTest);
        });
    document.getElementById('database-schedule-times').addEventListener('input', updateDatabaseSchedulePreview);
    document.getElementById('database-schedule-interval').addEventListener('input', updateDatabaseSchedulePreview);

    // ── Settings ──────────────────────────────────────────────
    document.getElementById('btn-save-settings').addEventListener('click', saveSettings);
    document.getElementById('btn-test-notification').addEventListener('click', testNotification);
    document.getElementById('btn-remove-webhook').addEventListener('click', removeWebhook);
    document.getElementById('btn-toggle-webhook-visibility').addEventListener('click', toggleWebhookVisibility);
    document.getElementById('btn-open-organization-enrollment').addEventListener('click', openOrganizationEnrollment);
    document.getElementById('btn-connect-account-organization').addEventListener('click', () => {
        void connectAccountOrganizationInstallation();
    });

    const workspaceTrigger = document.getElementById('workspace-trigger');
    const workspaceMenu = document.getElementById('workspace-menu');
    workspaceTrigger.addEventListener('click', () => {
        if (workspaceSwitchInProgress) return;
        const open = workspaceTrigger.getAttribute('aria-expanded') === 'true';
        setWorkspaceMenuOpen(!open);
    });
    workspaceMenu.addEventListener('click', (event) => {
        const option = event.target.closest('[data-workspace-id]');
        if (!option || option.disabled) return;
        void switchWorkspace(option.dataset.workspaceId);
    });
    document.addEventListener('click', (event) => {
        if (!event.target.closest('.workspace-switcher')) setWorkspaceMenuOpen(false);
    });
    document.addEventListener('keydown', (event) => {
        if (event.key === 'Escape') setWorkspaceMenuOpen(false);
    });
    document.getElementById('btn-paste-organization-token').addEventListener('click', () => {
        void pasteOrganizationSetupToken();
    });
    document.getElementById('organization-enrollment-form').addEventListener('submit', (event) => {
        event.preventDefault();
        void reviewOrganizationEnrollment();
    });
    document.getElementById('organization-setup-token').addEventListener('input', resetOrganizationEnrollmentPreview);
    document.getElementById('btn-cancel-organization-enrollment').addEventListener('click', closeOrganizationEnrollment);
    document.getElementById('btn-confirm-organization-enrollment').addEventListener('click', () => {
        void confirmOrganizationEnrollment();
    });

    // ── Folder Management ─────────────────────────────────────────
    document.getElementById('btn-create-folder').addEventListener('click', () => {
        document.getElementById('new-folder-name').value = '';
        document.getElementById('new-folder-location').textContent = currentFolder === '/'
            ? 'Creating in / (Root)'
            : `Creating in ${currentFolder}`;
        document.getElementById('new-folder-modal').classList.remove('hidden');
    });

    document.getElementById('btn-cancel-new-folder').addEventListener('click', () => {
        document.getElementById('new-folder-modal').classList.add('hidden');
    });

    document.getElementById('btn-confirm-new-folder').addEventListener('click', async () => {
        const name = document.getElementById('new-folder-name').value.trim();
        if (!name) { showToast('Please enter a folder name', 'error'); return; }
        const button = document.getElementById('btn-confirm-new-folder');
        try {
            button.disabled = true;
            await invoke('cmd_create_folder', { name, parentFolder: currentFolder });
            showToast(`Folder "${name}" created`, 'success');
            document.getElementById('new-folder-modal').classList.add('hidden');
            loadBackups();
        } catch (err) {
            showToast('Failed to create folder: ' + String(err), 'error');
        } finally {
            button.disabled = false;
        }
    });

    // ── Move Backup Modal ─────────────────────────────────────────
    document.getElementById('btn-cancel-move').addEventListener('click', () => {
        document.getElementById('move-backup-modal').classList.add('hidden');
    });

    document.getElementById('btn-confirm-move').addEventListener('click', async () => {
        const key = document.getElementById('move-backup-modal').dataset.backupKey;
        const destinationFolder = document.getElementById('move-dest-folder').value;
        if (!key) return;
        const button = document.getElementById('btn-confirm-move');
        try {
            button.disabled = true;
            button.textContent = 'Moving…';
            await invoke('cmd_move_backup', { key, destinationFolder });
            showToast('Backup moved successfully', 'success');
            document.getElementById('move-backup-modal').classList.add('hidden');
            await loadBackups();
        } catch (err) {
            showToast('Failed to move: ' + String(err), 'error');
        } finally {
            button.disabled = false;
            button.textContent = 'Move';
        }
    });
}

async function handleVaultLoginResult(result) {
    const mode = vaultRecoveryUi.classifyLoginResult(result);
    if (mode === 'ready') {
        const message = result && result.message;
        pendingVaultLoginResult = null;
        clearVaultFlowSecrets();
        showLoginAuthCard();
        await checkAuthStatus();
        if (message) showToast(message, 'info');
        return;
    }
    if (mode === 'setup') {
        pendingVaultLoginResult = result;
        const recoveryKey = vaultRecoveryUi.consumeOneTimeVaultRecoveryKey(result);
        if (!recoveryKey) throw new Error('The one-time vault recovery key was not returned. Vault setup was not committed.');
        document.querySelector('.login-card:not(#vault-recovery-setup):not(#vault-locked-card)').classList.add('hidden');
        document.getElementById('vault-locked-card').classList.add('hidden');
        document.getElementById('vault-recovery-key').textContent = recoveryKey;
        document.getElementById('vault-recovery-setup').classList.remove('hidden');
        return;
    }
    if (mode === 'locked') {
        pendingVaultLoginResult = result;
        document.querySelector('.login-card:not(#vault-recovery-setup):not(#vault-locked-card)').classList.add('hidden');
        document.getElementById('vault-recovery-setup').classList.add('hidden');
        document.getElementById('vault-locked-message').textContent = result.message || 'Your account is signed in, but the encrypted vault still needs a client-owned unlock factor.';
        const select = document.getElementById('vault-unlock-method');
        select.replaceChildren(...vaultRecoveryUi.unlockOptions(result).map((option) => {
            const element = document.createElement('option');
            element.value = option.value;
            element.textContent = option.label;
            return element;
        }));
        updateVaultUnlockLabel();
        document.getElementById('vault-locked-card').classList.remove('hidden');
        return;
    }
    throw new Error('The desktop returned an unknown vault state. No encrypted key was changed.');
}

function updateVaultUnlockLabel() {
    const method = document.getElementById('vault-unlock-method').value;
    const label = document.getElementById('vault-unlock-secret-label');
    const input = document.getElementById('vault-unlock-secret');
    const isRecoveryKey = method === 'vault_recovery_key';
    label.textContent = isRecoveryKey ? 'Offline vault recovery key' : 'Previous vault password';
    input.type = isRecoveryKey ? 'text' : 'password';
    input.placeholder = isRecoveryKey ? 'Paste the 256-bit offline key' : 'Password that previously unlocked this vault';
}

function clearVaultFlowSecrets() {
    vaultRecoveryUi.clearSensitiveValue(document.getElementById('vault-recovery-key'));
    vaultRecoveryUi.clearSensitiveValue(document.getElementById('vault-setup-account-password'));
    vaultRecoveryUi.clearSensitiveValue(document.getElementById('vault-unlock-secret'));
    vaultRecoveryUi.clearSensitiveValue(document.getElementById('vault-current-account-password'));
    document.getElementById('vault-recovery-ack').checked = false;
}

function showLoginAuthCard() {
    document.querySelector('.login-card:not(#vault-recovery-setup):not(#vault-locked-card)').classList.remove('hidden');
    document.getElementById('vault-recovery-setup').classList.add('hidden');
    document.getElementById('vault-locked-card').classList.add('hidden');
}

// ────────────────────────────────────────────────────────────────
// Tauri Event Listeners
// ────────────────────────────────────────────────────────────────
function setupTauriListeners() {
    listen('backup-progress', (event) => {
        const p = event.payload;
        const pct = Math.round(p.progress * 100);

        // Quick backup specific UI
        const fill = document.getElementById('backup-progress-fill');
        const msg = document.getElementById('backup-progress-msg');
        if (fill) fill.style.width = `${pct}%`;
        if (msg) msg.textContent = p.message;

        // Global progress UI
        const globalBar = document.getElementById('global-progress-bar');
        const globalFill = document.getElementById('global-progress-fill');
        const globalMsg = document.getElementById('global-progress-msg');
        const globalPct = document.getElementById('global-progress-pct');

        if (globalBar && globalFill && globalMsg && globalPct) {
            if (globalProgressHideTimer) {
                clearTimeout(globalProgressHideTimer);
                globalProgressHideTimer = null;
            }
            globalBar.classList.remove('hidden');
            globalFill.style.width = `${pct}%`;
            globalMsg.textContent = p.message;
            globalPct.textContent = `${pct}%`;

            if (p.stage === 'done') {
                globalProgressHideTimer = setTimeout(hideGlobalProgress, 2000);
            } else if (p.stage === 'error' || p.stage === 'cancelled') {
                hideGlobalProgress();
            }
        }

        // ── Profile card inline progress ──
        // Update ALL profile cards that have visible progress bars
        // (we don't know which profile ID emitted this, so update all active ones)
        document.querySelectorAll('.profile-progress:not(.hidden)').forEach(el => {
            const msgEl = el.querySelector('.profile-progress-msg');
            const pctEl = el.querySelector('.profile-progress-pct');
            const fillEl = el.querySelector('.profile-progress-fill');
            if (msgEl) msgEl.textContent = p.message;
            if (pctEl) pctEl.textContent = `${pct}%`;
            if (fillEl) fillEl.style.width = `${pct}%`;
        });

        if (p.stage === 'done') {
            showToast('Backup completed!', 'success');
            setTimeout(resetBackupMode, 2000);
            loadBackups();
            // Reload profiles to update "Last Run" and reset progress
            if (document.getElementById('page-profiles').classList.contains('active')) {
                loadProfiles();
            } else {
                // Even if not on profiles page, hide inline progress bars
                document.querySelectorAll('.profile-progress').forEach(el => el.classList.add('hidden'));
                document.querySelectorAll('.profile-actions .btn-primary').forEach(btn => {
                    btn.disabled = false;
                    btn.textContent = '▶ Run Now';
                });
            }
        } else if (p.stage === 'cancelled') {
            if (pendingBackupErrorToast) {
                clearTimeout(pendingBackupErrorToast);
                pendingBackupErrorToast = null;
            }
            resetBackupMode();
            showToast('Backup stopped. No cancelled backup was added.', 'info');
            document.querySelectorAll('.profile-progress').forEach(el => el.classList.add('hidden'));
            document.querySelectorAll('.profile-actions .btn-primary').forEach(btn => {
                btn.disabled = false;
                btn.textContent = '▶ Run Now';
            });
        } else if (p.stage === 'error') {
            resetBackupMode();
            if (pendingBackupErrorToast) clearTimeout(pendingBackupErrorToast);
            // Manual invokes reject immediately after this event and replace
            // the generic message with the actionable native error. Scheduled
            // failures have no invoking screen, so retain this fallback.
            pendingBackupErrorToast = setTimeout(() => {
                pendingBackupErrorToast = null;
                handleRepositoryError(p.message);
            }, 100);
            // Reset profile card progress on error
            document.querySelectorAll('.profile-progress').forEach(el => el.classList.add('hidden'));
            document.querySelectorAll('.profile-actions .btn-primary').forEach(btn => {
                btn.disabled = false;
                btn.textContent = '▶ Run Now';
            });
        }
    });

    listen('database-progress', (event) => {
        const progress = event.payload;
        const row = document.querySelector(`.database-row[data-database-profile-id="${cssEscape(progress.profileId)}"]`);
        if (row) {
            const wrap = row.querySelector('.database-row-progress');
            const message = row.querySelector('.database-progress-message');
            const percent = row.querySelector('.database-progress-percent');
            const fill = row.querySelector('.database-progress-fill');
            wrap?.classList.remove('hidden');
            const pct = Math.round(Number(progress.progress || 0) * 100);
            if (message) message.textContent = progress.message;
            if (percent) percent.textContent = `${pct}%`;
            if (fill) fill.style.width = `${pct}%`;
        }
        if (['done', 'cancelled', 'error'].includes(progress.stage)) {
            if (progress.stage === 'done' && document.getElementById('page-databases').classList.contains('active')) {
                setTimeout(() => loadDatabaseProfiles(), 900);
            } else if (row) {
                row.querySelector('.database-row-progress')?.classList.add('hidden');
                const run = row.querySelector('[data-database-action="run"]');
                if (run) {
                    run.disabled = false;
                    run.textContent = 'Run Now';
                }
            }
        }
    });

    listen('database-restore-progress', (event) => {
        const progress = event.payload;
        const row = document.querySelector(`.database-row[data-database-profile-id="${cssEscape(progress.profileId)}"]`);
        if (!row) return;
        const pct = Math.round(Number(progress.progress || 0) * 100);
        row.querySelector('.database-row-progress')?.classList.remove('hidden');
        const message = row.querySelector('.database-progress-message');
        const percent = row.querySelector('.database-progress-percent');
        const fill = row.querySelector('.database-progress-fill');
        if (message) message.textContent = progress.message;
        if (percent) percent.textContent = `${pct}%`;
        if (fill) fill.style.width = `${pct}%`;
        if (['done', 'cancelled', 'error'].includes(progress.stage)) {
            setTimeout(() => row.querySelector('.database-row-progress')?.classList.add('hidden'), 1200);
        }
    });

    listen('restore-progress', (event) => {
        const p = event.payload;

        // Update whichever restore modal is visible
        const fills = ['restore-progress-fill', 'selective-progress-fill'];
        const msgs = ['restore-progress-msg', 'selective-progress-msg'];
        fills.forEach(id => {
            const el = document.getElementById(id);
            if (el) el.style.width = `${Math.round(p.progress * 100)}%`;
        });
        msgs.forEach(id => {
            const el = document.getElementById(id);
            if (el) el.textContent = p.message;
        });

        if (p.stage === 'done') {
            showToast('Restore completed!', 'success');
            resetRestoreModal();
            document.getElementById('selective-restore-modal').classList.add('hidden');
        } else if (p.stage === 'cancelled') {
            showToast('Restore cancelled. Partial files were removed.', 'info');
            resetRestoreModal();
            document.getElementById('selective-restore-modal').classList.add('hidden');
        } else if (p.stage === 'error') {
            if (pendingRestoreErrorToast) clearTimeout(pendingRestoreErrorToast);
            pendingRestoreErrorToast = setTimeout(() => {
                pendingRestoreErrorToast = null;
                handleRepositoryError(p.message);
            }, 100);
            resetRestoreModal();
            document.getElementById('selective-restore-modal').classList.add('hidden');
        }
    });

    listen('navigate', (event) => {
        navigateTo(event.payload);
    });

    listen('storage-cleanup', (event) => {
        const cleanup = event.payload;
        storageCleanupPending = cleanup.status === 'pending' || cleanup.status === 'running';
        setStorageCleanupState(
            storageCleanupPending,
            cleanup.status === 'pending' ? friendlyError(cleanup.message) : cleanup.message,
        );

        if (cleanup.status === 'complete') {
            showToast('Deleted storage cleanup completed', 'success');
            setTimeout(() => {
                if (document.getElementById('page-dashboard').classList.contains('active')) {
                    loadDashboard();
                }
            }, 1200);
        } else if (cleanup.status === 'failed') {
            showToast(friendlyError(cleanup.message), 'error');
        }
    });

    listen('update-progress', (event) => {
        const progress = event.payload;
        const progressArea = document.getElementById('update-progress-area');
        const progressFill = document.getElementById('update-progress-fill');
        const progressPct = document.getElementById('update-progress-pct');

        progressArea.classList.remove('hidden');
        if (progress.stage === 'started') {
            updatePhase = 'downloading';
            updateProgressPercent = 0;
            updateStatusMessage = `Downloading version ${progress.version}…`;
            renderUpdaterUi();
            return;
        }

        if (progress.stage === 'progress' && progress.total > 0) {
            const pct = Math.min(100, Math.round((progress.downloaded / progress.total) * 100));
            updatePhase = 'downloading';
            updateProgressPercent = pct;
            updateStatusMessage = `Downloading version ${progress.version}, ${pct}%`;
            progressFill.style.width = `${pct}%`;
            progressPct.textContent = `${pct}%`;
            renderUpdaterUi();
        } else if (progress.stage === 'downloaded') {
            updatePhase = 'verifying';
            updateProgressPercent = 100;
            updateStatusMessage = 'Download complete. Verifying the update…';
            progressFill.style.width = '100%';
            progressPct.textContent = '100%';
            renderUpdaterUi();
        }
    });
}

// ────────────────────────────────────────────────────────────────
// Auth
// ────────────────────────────────────────────────────────────────
async function checkAuthStatus() {
    try {
        const result = await invoke('cmd_get_auth_status');
        if (result && result.authenticated && result.master_key_ready) {
            if (!authenticatedSessionActive) repositorySessionGeneration += 1;
            authenticatedSessionActive = true;
            showView('app');
            warmRepositoryInBackground();
            loadDashboard();
            loadSettings();
            if (!legacyProfileNoticeShown) {
                legacyProfileNoticeShown = true;
                invoke('cmd_count_unowned_profiles')
                    .then((count) => {
                        if (Number(count) > 0) {
                            showToast('Existing profiles are paused until you assign them to an account from the Profiles page.', 'info');
                        }
                    })
                    .catch(() => {});
            }
        } else {
            // Token expired or master key missing - need fresh login
            endAuthenticatedSession();
            showView('login');
        }
    } catch {
        endAuthenticatedSession();
        showView('login');
    }
}

function endAuthenticatedSession() {
    authenticatedSessionActive = false;
    currentAccount = null;
    repositorySessionGeneration += 1;
    repositoryWarmupPromise = null;
    legacyProfileNoticeShown = false;
    accountWorkspaces = [];
    workspaceSwitchInProgress = false;
    workspaceUiGeneration += 1;
    setWorkspaceMenuOpen(false);
}

function setWorkspaceMenuOpen(open) {
    const trigger = document.getElementById('workspace-trigger');
    const menu = document.getElementById('workspace-menu');
    if (!trigger || !menu) return;
    trigger.setAttribute('aria-expanded', open ? 'true' : 'false');
    menu.classList.toggle('hidden', !open);
}

function workspaceIcon(workspace) {
    return workspace.kind === 'organization'
        ? '<svg width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" viewBox="0 0 24 24" aria-hidden="true"><path d="M3 21h18M5 21V7l7-4 7 4v14M9 10h1m4 0h1m-6 4h1m4 0h1m-6 4h6"/></svg>'
        : '<svg width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0116 0"/></svg>';
}

function renderWorkspaceSwitcher() {
    const current = accountWorkspaces.find((workspace) => workspace.current) || accountWorkspaces[0];
    document.getElementById('workspace-current-label').textContent = current?.label || 'Workspace';
    document.getElementById('workspace-current-kind').textContent = current
        ? (current.kind === 'organization' ? 'Organization' : current.plan || 'Personal')
        : 'Unavailable';
    const menu = document.getElementById('workspace-menu');
    menu.innerHTML = accountWorkspaces.map((workspace) => `
        <button type="button" class="workspace-option" role="option"
                data-workspace-id="${escapeHtml(workspace.id)}"
                aria-selected="${workspace.current ? 'true' : 'false'}"
                ${workspace.available && !workspaceSwitchInProgress ? '' : 'disabled'}>
            <span class="workspace-option-icon">${workspaceIcon(workspace)}</span>
            <span class="workspace-option-copy">
                <strong>${escapeHtml(workspace.label)}</strong>
                <small>${escapeHtml(workspace.kind === 'organization' ? workspace.plan : `Private · ${workspace.plan}`)}</small>
            </span>
            <span class="workspace-option-check">${workspace.current ? '✓' : ''}</span>
        </button>
    `).join('');
}

async function switchWorkspace(workspaceId) {
    const target = accountWorkspaces.find((workspace) => workspace.id === workspaceId);
    if (!target || target.current || !target.available || workspaceSwitchInProgress) {
        setWorkspaceMenuOpen(false);
        return;
    }
    workspaceSwitchInProgress = true;
    renderWorkspaceSwitcher();
    try {
        await invoke('cmd_switch_account_workspace', { workspaceId });
        workspaceUiGeneration += 1;
        repositorySessionGeneration += 1;
        repositoryWarmupPromise = null;
        currentAccount = null;
        currentFolder = '/';
        folderList = [];
        setWorkspaceMenuOpen(false);
        showToast(`Switched to ${target.label}.`, 'success');
        await Promise.all([
            loadDashboard(),
            loadSettings(),
            loadProfiles(),
            loadDatabaseProfiles(),
            loadBackups(),
        ]);
        warmRepositoryInBackground();
    } catch (error) {
        showToast('Could not switch workspace: ' + friendlyError(error), 'error');
    } finally {
        workspaceSwitchInProgress = false;
        renderWorkspaceSwitcher();
    }
}

function warmRepositoryInBackground() {
    if (!authenticatedSessionActive || repositoryWarmupPromise) return;

    const sessionGeneration = repositorySessionGeneration;
    const warmup = invoke('cmd_warm_repository')
        .catch((error) => {
            console.warn('Repository warm-up did not complete:', error);
            if (authenticatedSessionActive
                && repositorySessionGeneration === sessionGeneration
                && isRepositoryKeyMismatch(error)) {
                return handleRepositoryError(error);
            }
        })
        .finally(() => {
            if (repositoryWarmupPromise === warmup) {
                repositoryWarmupPromise = null;
            }
        });
    repositoryWarmupPromise = warmup;
}

// ────────────────────────────────────────────────────────────────
// View / Page Managemen
// ────────────────────────────────────────────────────────────────
function showView(viewId) {
    document.getElementById('view-login').classList.toggle('active', viewId === 'login');
    document.getElementById('view-app').classList.toggle('active', viewId === 'app');
}

function navigateTo(pageId) {
    Object.values(pages).forEach(p => { if (p) p.classList.remove('active'); });
    const target = pages[pageId];
    if (target) target.classList.add('active');

    document.querySelectorAll('.nav-link').forEach(l => {
        l.classList.toggle('active', l.getAttribute('data-view') === pageId);
    });

    if (pageId === 'dashboard') loadDashboard();
    if (pageId === 'backups') loadBackups();
    if (pageId === 'profiles') loadProfiles();
    if (pageId === 'databases') loadDatabaseProfiles();
    if (pageId === 'settings') loadSettings();
}

// ────────────────────────────────────────────────────────────────
// Toas
// ────────────────────────────────────────────────────────────────
function friendlyError(error) {
    const raw = String(error ?? '').replace(/\\n/g, ' ').replace(/[\r\n\t]+/g, ' ').trim();
    const lower = raw.toLowerCase();

    if (lower.includes('message authentication failed') ||
        lower.includes('unable to decrypt content') ||
        lower.includes('invalid checksum') ||
        lower.includes('unable to load manifest content')) {
        return 'This backup repository cannot be decrypted with the account key stored on this PC. No data was changed. Sign in again to reconnect it.';
    }
    if (lower.includes('designated user')) {
        return 'Storage cleanup could not take ownership of this older repository. Your backups remain safe; install the latest update and try again.';
    }
    if (lower.includes('missing required key name') || lower.includes("invalid args `name`")) {
        return 'The folder name could not be sent to the app. Install the latest update and try again.';
    }
    if (lower.includes('folder_not_empty')) {
        return 'This folder is not empty. Move its backups and remove nested folders first.';
    }
    if (lower.includes('source_quota_exceeded') || lower.includes('backup data allowance exceeded')) {
        return 'This backup was blocked by an outdated source-data quota check. Update SaveState and try again.';
    }
    if (lower.includes('optimized_storage_quota_exceeded')) {
        return raw.split(':').slice(1).join(':').trim() || 'Your optimized backup storage is full after cleanup. Remove an older backup or choose a larger plan, then try again.';
    }
    if (lower.includes('quotaexceeded') || lower.includes('storage limit exceeded')) {
        return 'Your encrypted repository reached its temporary safety ceiling. Run storage cleanup, remove an older backup, or choose a larger plan, then try again.';
    }
    if (lower.includes('database_authentication_failed')) {
        return 'The database rejected that username or password. Check the credentials and test the connection again.';
    }
    if (lower.includes('database_unreachable')) {
        return 'SaveState could not reach the database. Check that MySQL or MariaDB is running and that the host and port are correct.';
    }
    if (lower.includes('setup_token_expired')) {
        return 'This setup token has expired. Ask the organization for a new token.';
    }
    if (lower.includes('setup_token_used')) {
        return 'This setup token has already been used. Ask the organization for a replacement if this PC is not connected.';
    }
    if (lower.includes('customer_approval_required')) {
        return 'Approve the organization request for this account before connecting the installation.';
    }
    if (lower.includes('installation_disabled')) {
        return 'This organization installation has been disabled. Contact the organization before trying again.';
    }
    if (lower.includes('installation_not_found') || lower.includes('organization_assignment_not_found')) {
        return 'This organization installation is no longer assigned to your account. Refresh or contact the organization.';
    }
    if (lower.includes('installation_already_connected')) {
        return 'This organization installation is already connected to another SaveState app.';
    }
    if (lower.includes('device_already_connected')) {
        return 'This SaveState app is already connected to an organization installation.';
    }
    if (lower.includes('storage_service_unavailable') || lower.includes('organization_service_unavailable')) {
        return 'The organization storage service is not ready yet. Contact the organization and try again.';
    }
    if (lower.includes('organization_service_provisioning_failed')) {
        return 'SaveState could not prepare the organization storage right now. Try again shortly.';
    }
    if (lower.includes('database_tool_not_found') || lower.includes('database_tool_invalid')) {
        return 'The configured MySQL or MariaDB tools could not be verified. Scan this PC again or choose their executable locations.';
    }
    if (lower.includes('database_grants_unsupported')) {
        return 'This dump tool cannot export users and grants. Choose a compatible MariaDB tool or turn that option off.';
    }
    if (lower.includes('database_export_failed')) {
        return raw.split(':').slice(1).join(':').trim() || 'The database export failed before SaveState committed a backup.';
    }
    if (lower.includes('database_restore_failed')) {
        return raw.split(':').slice(1).join(':').trim() || 'The database rejected part of the SQL restore. Review the destination before retrying.';
    }

    let message = raw;
    const jsonMatch = raw.match(/\{\s*"(?:error|message)"\s*:\s*"([^"]+)"/i);
    if (jsonMatch) message = jsonMatch[1];
    if (!message) message = 'Something went wrong. Please try again.';
    return message.length > 240 ? `${message.slice(0, 237)}…` : message;
}

function isRepositoryKeyMismatch(error) {
    const lower = String(error ?? '').toLowerCase();
    return lower.includes('repository_key_mismatch') ||
        lower.includes('message authentication failed') ||
        lower.includes('unable to decrypt content') ||
        lower.includes('invalid checksum') ||
        lower.includes('unable to load manifest content');
}

async function handleRepositoryError(error) {
    if (!isRepositoryKeyMismatch(error)) {
        showToast(error, 'error');
        return;
    }

    showToast(error, 'error');
    if (repositoryRecoveryPromptOpen) return;
    repositoryRecoveryPromptOpen = true;
    let preparedLogout = null;

    try {
        const signInAgain = await confirmDialog(
            'SaveState cannot open this repository with the account key currently stored on this PC. Sign in again to reload the correct account key and repository. No backup data will be changed.',
            { title: 'Reconnect encrypted repository', kind: 'warning' },
        );
        if (!signInAgain) return;

        preparedLogout = await invoke('cmd_prepare_logout');
        const activeBackups = preparedLogout.activeBackups || [];
        const stopBackups = await window.SaveStateLogout.confirmActiveBackups(
            activeBackups,
            confirmDialog,
        );
        if (!stopBackups) {
            await invoke('cmd_abort_logout', { logoutToken: preparedLogout.token });
            preparedLogout = null;
            return;
        }
        await invoke('cmd_logout', { logoutToken: preparedLogout.token });
        preparedLogout = null;
        endAuthenticatedSession();
        pendingVaultLoginResult = null;
        clearVaultFlowSecrets();
        showLoginAuthCard();
        document.getElementById('login-password').value = '';
        const errorEl = document.getElementById('login-error');
        errorEl.textContent = 'Sign in again to reconnect your encrypted backup repository.';
        errorEl.classList.remove('hidden');
        showView('login');
    } catch (recoveryError) {
        if (preparedLogout) {
            await invoke('cmd_abort_logout', { logoutToken: preparedLogout.token }).catch(() => {});
        }
        showToast('Could not restart sign-in: ' + String(recoveryError), 'error');
    } finally {
        repositoryRecoveryPromptOpen = false;
    }
}

function showToast(message, type = 'success') {
    const container = document.getElementById('toast-container');
    const displayMessage = type === 'error' ? friendlyError(message) : String(message);
    const key = `${type}:${displayMessage}`;
    if (activeToasts.has(key)) return;

    while (container.children.length >= 3) {
        const oldest = container.firstElementChild;
        if (!oldest) break;
        clearTimeout(oldest._removeTimer);
        activeToasts.delete(oldest.dataset.toastKey);
        oldest.remove();
    }

    const toast = document.createElement('div');
    toast.className = `toast ${type}`;
    toast.dataset.toastKey = key;
    toast.textContent = displayMessage;
    container.appendChild(toast);
    activeToasts.set(key, toast);
    toast._removeTimer = setTimeout(() => {
        activeToasts.delete(key);
        toast.remove();
    }, type === 'error' ? 7000 : 3500);
}

// ────────────────────────────────────────────────────────────────
// Dashboard
// ────────────────────────────────────────────────────────────────
async function loadDashboard() {
    const generation = workspaceUiGeneration;
    try {
        const [workspaceResponse, account, backupState] = await Promise.all([
            invoke('cmd_list_account_workspaces'),
            invoke('cmd_get_account'),
            invoke('cmd_list_backups').catch(() => null),
        ]);
        if (generation !== workspaceUiGeneration) return;
        accountWorkspaces = Array.isArray(workspaceResponse?.workspaces)
            ? workspaceResponse.workspaces
            : [];
        renderWorkspaceSwitcher();
        currentAccount = account;
        document.getElementById('sidebar-email').textContent = account.email || '';
        document.getElementById('stat-email').textContent = account.email || '';
        document.getElementById('stat-email').title = account.email || '';
        document.getElementById('stat-plan').textContent = account.plan || 'No plan';

        const sourceStatistics = storageUsageUi.sourceStatistics(account.usage, backupState);
        const usage = storageUsageUi.customerVisibleUsage(account.usage, backupState);
        const limitGB = account.storageLimitGb || account.storageLimitGB || account.storage_limit_gb || 0;
        const reportedLimitBytes = Number(account.usage?.limitBytes);
        const limitBytes = Number.isSafeInteger(reportedLimitBytes) && reportedLimitBytes >= 0
            ? reportedLimitBytes
            : limitGB * 1024 * 1024 * 1024;
        const pct = usage !== null && limitBytes > 0
            ? Math.min(100, (usage / limitBytes) * 100)
            : 0;
        const backupValue = document.getElementById('usage-backup');
        const retained = sourceStatistics.snapshotCount === null
            ? 'retained backups'
            : `${sourceStatistics.snapshotCount.toLocaleString()} retained ${sourceStatistics.snapshotCount === 1 ? 'backup' : 'backups'}`;
        const files = sourceStatistics.fileCount === null
            ? ''
            : ` · ${sourceStatistics.fileCount.toLocaleString()} files`;
        if (usage === null) {
            backupValue.textContent = 'Calculating…';
            backupValue.title = 'Optimized encrypted storage is being measured';
            document.getElementById('usage-backup-meta').textContent = `${retained}${files}`;
        } else {
            backupValue.textContent = limitBytes > 0
                ? `${formatBytes(usage)} of ${formatBytes(limitBytes)}`
                : formatBytes(usage);
            backupValue.title = `${usage.toLocaleString()} optimized encrypted repository bytes${limitBytes > 0 ? ` of ${limitBytes.toLocaleString()} bytes` : ''}`;
            const sourceSummary = sourceStatistics.sourceBytes === null
                ? 'Source data measurement pending'
                : `${formatBytes(sourceStatistics.sourceBytes)} source data protected`;
            document.getElementById('usage-backup-meta').textContent =
                `${usage.toLocaleString()} optimized bytes · ${sourceSummary} · ${retained}${files}`;
        }
        document.getElementById('backup-fill').style.width = `${pct}%`;
        document.getElementById('backup-pct').textContent = `${pct < 1 && pct > 0 ? pct.toFixed(2) : Math.round(pct)}%`;

        const uploadUsed = Math.max(0, Math.trunc(Number(account.ingress?.used || 0)));
        const uploadValue = document.getElementById('usage-upload');
        uploadValue.textContent = formatBytes(uploadUsed);
        uploadValue.title = `${uploadUsed.toLocaleString()} original source bytes backed up this month`;
        document.getElementById('usage-upload-meta').textContent =
            `This month · ${uploadUsed.toLocaleString()} original source bytes · free`;

        const restoreUsed = Math.max(0, Math.trunc(Number(account.egress?.used || 0)));
        const restoreValue = document.getElementById('usage-restore');
        restoreValue.textContent = `${formatBytes(restoreUsed)} this month`;
        restoreValue.title = `${restoreUsed.toLocaleString()} encrypted bytes restored this month`;
        document.getElementById('usage-restore-meta').textContent =
            `${restoreUsed.toLocaleString()} bytes this month · unlimited · free`;

        const inferredCleanup = storageUsageUi.shouldScheduleCleanup(account.usage, backupState);
        // A maintenance hint is not a customer-facing storage state. Only show
        // the badge while a native cleanup job is actually queued or running.
        setStorageCleanupState(storageCleanupPending);
        if (inferredCleanup) {
            invoke('cmd_schedule_storage_cleanup').catch((error) => {
                console.warn('Could not schedule storage cleanup:', error);
            });
        }

        // Subscription status
        const subCard = document.getElementById('sub-status-card');
        const status = account.status || 'active';
        const statusText = document.getElementById('sub-status-text');
        const renewsText = document.getElementById('sub-renews-text');
        const resumeBtn = document.getElementById('btn-resume-sub');

        subCard.classList.remove('hidden');
        statusText.textContent = status.charAt(0).toUpperCase() + status.slice(1);
        statusText.className = 'sub-status-value status-' + status;

        if (account.currentPeriodEnd || account.current_period_end) {
            const end = account.currentPeriodEnd || account.current_period_end;
            const endDate = new Date(end * 1000 || end);
            renewsText.textContent = status === 'cancelling'
                ? `Ends ${endDate.toLocaleDateString()}`
                : `${endDate.toLocaleDateString()}`;
        } else if (account.trialEndsAt || account.trial_ends_at) {
            const trial = account.trialEndsAt || account.trial_ends_at;
            const trialDate = new Date(trial);
            renewsText.textContent = `Trial ends ${trialDate.toLocaleDateString()}`;
        } else {
            renewsText.textContent = '—';
        }

        // Show resume button only when cancelling
        if (status === 'cancelling') {
            resumeBtn.classList.remove('hidden');
        } else {
            resumeBtn.classList.add('hidden');
        }
    } catch (e) {
        console.error('loadDashboard error:', e);
        if (String(e).includes('401') || String(e).includes('Unauthorized') || String(e).includes('Not authenticated')) {
            // Do not invoke cmd_logout here. An auth_version change invalidates
            // the token, but Credential Manager may hold the only AMK capable
            // of recovering this client-owned vault after account reset.
            await invoke('cmd_abandon_vault_unlock').catch(() => {});
            endAuthenticatedSession();
            showLoginAuthCard();
            showView('login');
        } else {
            showToast('Failed to load account: ' + String(e), 'error');
        }
    }
}

function setStorageCleanupState(pending, message = 'Cleanup pending') {
    const state = document.getElementById('cleanup-state');
    const text = document.getElementById('cleanup-state-text');
    if (!state || !text) return;
    state.classList.toggle('hidden', !pending);
    text.textContent = pending ? (message || 'Cleanup pending') : '';
}

// ────────────────────────────────────────────────────────────────
// Backup helpers
// ────────────────────────────────────────────────────────────────
function startBackupMode() {
    document.querySelector('.backup-options')?.classList.add('hidden');
    document.getElementById('backup-progress-area')?.classList.remove('hidden');
    const fill = document.getElementById('backup-progress-fill');
    if (fill) fill.style.width = '0%';
    const msg = document.getElementById('backup-progress-msg');
    if (msg) msg.textContent = 'Starting…';
}

function hideGlobalProgress() {
    if (globalProgressHideTimer) {
        clearTimeout(globalProgressHideTimer);
        globalProgressHideTimer = null;
    }
    document.getElementById('global-progress-bar')?.classList.add('hidden');
}

function failBackupUi(error) {
    if (pendingBackupErrorToast) {
        clearTimeout(pendingBackupErrorToast);
        pendingBackupErrorToast = null;
    }
    hideGlobalProgress();
    resetBackupMode();
    document.querySelectorAll('.profile-progress').forEach((element) => element.classList.add('hidden'));
    document.querySelectorAll('.profile-actions .btn-primary').forEach((button) => {
        button.disabled = false;
        button.textContent = '▶ Run Now';
    });
    if (String(error ?? '').toLowerCase().includes('backup_cancelled')) return;
    handleRepositoryError(error);
}

function resetBackupMode() {
    document.querySelector('.backup-options')?.classList.remove('hidden');
    document.getElementById('backup-progress-area')?.classList.add('hidden');
}

// ────────────────────────────────────────────────────────────────
// Backups Lis
// ────────────────────────────────────────────────────────────────
function folderContainsPath(parent, candidate) {
    const normalizedParent = parent || '/';
    const normalizedCandidate = candidate || '/';
    return normalizedCandidate === normalizedParent
        || (normalizedParent !== '/' && normalizedCandidate.startsWith(`${normalizedParent}/`));
}

async function loadBackups() {
    const tbody = document.getElementById('backups-tbody');
    const folderGrid = document.getElementById('folder-grid');
    if (!tbody) return;

    try {
        const data = await invoke('cmd_list_backups');
        const allBackups = data.backups || [];

        // Try loading folders
        let folders = [];
        try {
            const folderData = await invoke('cmd_list_folders');
            folders = folderData.folders || folderData || [];
        } catch {
            // cmd_list_folders may not exist yet - that's OK
        }

        // Store folder list globally for move/profile dropdowns
        folderList = folders;
        updateFolderDropdowns();

        // Determine subfolders of currentFolder
        const subfolders = [];
        const seenFolders = new Set();

        folders.forEach(f => {
            const fPath = typeof f === 'string' ? f : f.path;
            if (!fPath) return;
            const normalized = fPath.startsWith('/') ? fPath : '/' + fPath;
            const parts = normalized.split('/').filter(Boolean);
            const currentParts = currentFolder === '/' ? [] : currentFolder.split('/').filter(Boolean);

            if (parts.length > currentParts.length) {
                const isChild = currentParts.every((p, i) => parts[i] === p);
                if (isChild) {
                    const childName = parts[currentParts.length];
                    if (!seenFolders.has(childName)) {
                        seenFolders.add(childName);
                        const childPath = '/' + [...currentParts, childName].join('/');
                        const itemCount = allBackups.filter(b => {
                            const bFolder = b.folder || '/';
                            return bFolder === childPath || bFolder.startsWith(childPath + '/');
                        }).length;
                        const managedFolder = folders.find((entry) => typeof entry !== 'string'
                            && entry.path === childPath && entry.managed);
                        subfolders.push({
                            name: childName,
                            path: childPath,
                            itemCount,
                            managed: Boolean(managedFolder),
                            profileId: managedFolder?.profileId || null,
                            profileName: managedFolder?.profileName || null,
                        });
                    }
                }
            }
        });

        // Also check backups for implicit folders
        allBackups.forEach(b => {
            const bFolder = b.folder || '/';
            const normalized = bFolder.startsWith('/') ? bFolder : '/' + bFolder;
            const parts = normalized.split('/').filter(Boolean);
            const currentParts = currentFolder === '/' ? [] : currentFolder.split('/').filter(Boolean);

            if (parts.length > currentParts.length) {
                const isChild = currentParts.every((p, i) => parts[i] === p);
                if (isChild) {
                    const childName = parts[currentParts.length];
                    if (!seenFolders.has(childName)) {
                        seenFolders.add(childName);
                        const childPath = '/' + [...currentParts, childName].join('/');
                        const itemCount = allBackups.filter(b2 => {
                            const f2 = b2.folder || '/';
                            return f2 === childPath || f2.startsWith(childPath + '/');
                        }).length;
                        subfolders.push({ name: childName, path: childPath, itemCount, managed: false });
                    }
                }
            }
        });

        // Render breadcrumb
        renderBackupsBreadcrumb();

        // Render folder grid
        folderGrid.innerHTML = '';
        subfolders.forEach(sf => {
            const card = document.createElement('div');
            card.className = 'folder-card';
            const openButton = document.createElement('button');
            openButton.type = 'button';
            openButton.className = 'folder-card-open';
            openButton.setAttribute('aria-label', `Open folder ${sf.name}`);
            openButton.innerHTML = `
                <span class="folder-card-icon">📁</span>
                <div class="folder-card-name" title="${escapeHtml(sf.name)}">${escapeHtml(sf.name)}</div>
                <div class="folder-card-count">${sf.itemCount} item${sf.itemCount !== 1 ? 's' : ''}</div>
                ${sf.managed ? `<div class="folder-card-managed">${escapeHtml(sf.profileName || 'Backup profile')}</div>` : ''}
            `;
            openButton.addEventListener('click', () => navigateToFolder(sf.path));
            card.appendChild(openButton);

            // Delete folder button
            const delBtn = document.createElement('button');
            delBtn.className = 'folder-card-delete';
            delBtn.textContent = '✕';
            delBtn.title = 'Delete folder';
            delBtn.setAttribute('aria-label', `Delete folder ${sf.name}`);
            delBtn.addEventListener('click', async (e) => {
                e.stopPropagation();
                const containedBackups = allBackups.filter((backup) => folderContainsPath(sf.path, backup.folder || '/'));
                const containedProfiles = folders.filter((entry) => typeof entry !== 'string'
                    && entry.managed && folderContainsPath(sf.path, entry.path));
                const profileWarning = containedProfiles.length > 0
                    ? ` This also removes ${containedProfiles.length} backup profile${containedProfiles.length === 1 ? '' : 's'} whose managed folder is inside it.`
                    : '';
                const confirmed = await confirmDialog(
                    `Permanently delete "${sf.name}" and all ${containedBackups.length} backup${containedBackups.length === 1 ? '' : 's'} inside it?${profileWarning} This cannot be undone.`,
                    { title: 'Delete folder and contents', kind: 'warning' },
                );
                if (!confirmed) return;
                try {
                    delBtn.disabled = true;
                    const result = await invoke('cmd_delete_folder', { name: sf.path });
                    showToast(`Deleted "${sf.name}", ${Number(result?.deletedSnapshots || 0)} backup${Number(result?.deletedSnapshots || 0) === 1 ? '' : 's'} and ${Number(result?.deletedProfiles || 0)} profile${Number(result?.deletedProfiles || 0) === 1 ? '' : 's'}.`, 'success');
                    loadBackups();
                } catch (err) {
                    showToast('Failed to delete folder: ' + String(err), 'error');
                    delBtn.disabled = false;
                }
            });
            card.appendChild(delBtn);
            folderGrid.appendChild(card);
        });

        // Filter backups for current folder only (not subfolders)
        const currentBackups = allBackups.filter(b => {
            const bFolder = b.folder || '/';
            return bFolder === currentFolder;
        }).sort((left, right) => new Date(right.lastModified) - new Date(left.lastModified));

        // Render backup table
        tbody.innerHTML = '';
        if (currentBackups.length === 0 && subfolders.length === 0) {
            const tr = document.createElement('tr');
            const td = document.createElement('td');
            td.colSpan = 4;
            td.className = 'text-muted text-center';
            td.textContent = currentFolder === '/' ? 'No backups yet' : 'This folder is empty';
            tr.appendChild(td);
            tbody.appendChild(tr);
            return;
        }

        if (currentBackups.length === 0) {
            const tr = document.createElement('tr');
            const td = document.createElement('td');
            td.colSpan = 4;
            td.className = 'text-muted text-center';
            td.textContent = 'No files in this folder';
            tr.appendChild(td);
            tbody.appendChild(tr);
            return;
        }

        currentBackups.forEach(b => {
            const tr = document.createElement('tr');
            const tdName = document.createElement('td');
            tdName.className = 'backup-name-cell';
            const filename = document.createElement('span');
            filename.textContent = b.filename;
            tdName.appendChild(filename);
            if (Number.isFinite(Number(b.versionNumber)) && Number(b.versionNumber) > 0) {
                const profileVersions = allBackups
                    .filter((backup) => backup.profileId === b.profileId && Number(backup.versionNumber) > 0)
                    .map((backup) => Number(backup.versionNumber));
                const version = Number(b.versionNumber);
                const oldestVersion = Math.min(...profileVersions);
                const newestVersion = Math.max(...profileVersions);
                const versionBadge = document.createElement('span');
                versionBadge.className = 'backup-version-badge';
                if (version === newestVersion) {
                    versionBadge.classList.add('is-newest');
                    versionBadge.textContent = `Newest · v${version}`;
                } else if (version === oldestVersion) {
                    versionBadge.textContent = `Oldest · v${version}`;
                } else {
                    versionBadge.textContent = `Version ${version}`;
                }
                tdName.appendChild(versionBadge);
            }
            const tdSize = document.createElement('td');
            tdSize.textContent = b.sizeFormatted;
            const tdDate = document.createElement('td');
            tdDate.textContent = new Date(b.lastModified).toLocaleString();
            const tdActions = document.createElement('td');
            const actionsDiv = document.createElement('div');
            actionsDiv.className = 'context-actions';

            // Restore button
            const restoreBtn = document.createElement('button');
            restoreBtn.className = 'btn btn-primary btn-sm btn-icon';
            restoreBtn.textContent = '↗';
            restoreBtn.title = b.backupKind === 'database' ? 'Restore from the Databases page' : 'Restore backup';
            restoreBtn.setAttribute('aria-label', restoreBtn.title);
            restoreBtn.addEventListener('click', () => {
                if (b.backupKind === 'database') {
                    navigateTo('databases');
                    showToast('Open this database connection’s restore points to import the snapshot.', 'info');
                    return;
                }
                openRestoreModal(b.key, b.filename);
            });

            // Move button
            const moveBtn = document.createElement('button');
            moveBtn.className = 'btn btn-ghost btn-sm btn-icon';
            moveBtn.textContent = '→';
            moveBtn.title = 'Move to folder';
            moveBtn.setAttribute('aria-label', `Move ${b.filename} to another folder`);
            moveBtn.addEventListener('click', () => openMoveModal(b));

            // Delete button
            const deleteBtn = document.createElement('button');
            deleteBtn.className = 'btn btn-danger btn-sm btn-icon';
            deleteBtn.textContent = '🗑';
            deleteBtn.title = 'Delete backup';
            deleteBtn.setAttribute('aria-label', `Delete backup ${b.filename}`);
            deleteBtn.addEventListener('click', async () => {
                const confirmed = await confirmDialog(
                    'Delete this backup permanently?',
                    { title: 'Delete backup', kind: 'warning' },
                );
                if (confirmed) {
                    try {
                        deleteBtn.disabled = true;
                        tr.remove();
                        showToast('Deleting backup…', 'info');
                        await invoke('cmd_delete_backup', { key: b.key });
                        showToast('Backup deleted. Storage cleanup queued.', 'success');
                        void loadBackups();
                    } catch (err) {
                        showToast(String(err), 'error');
                        if (!tr.isConnected) {
                            await loadBackups();
                        } else {
                            deleteBtn.disabled = false;
                        }
                    }
                }
            });

            actionsDiv.appendChild(restoreBtn);
            actionsDiv.appendChild(moveBtn);
            actionsDiv.appendChild(deleteBtn);
            tdActions.appendChild(actionsDiv);

            tr.appendChild(tdName);
            tr.appendChild(tdSize);
            tr.appendChild(tdDate);
            tr.appendChild(tdActions);
            tbody.appendChild(tr);
        });
    } catch (e) {
        tbody.innerHTML = '';
        const tr = document.createElement('tr');
        const td = document.createElement('td');
        td.colSpan = 4;
        td.className = 'text-muted text-center';
        td.textContent = friendlyError(e);
        tr.appendChild(td);
        tbody.appendChild(tr);
        handleRepositoryError(e);
    }
}

function renderBackupsBreadcrumb() {
    const container = document.getElementById('backups-breadcrumb');
    container.innerHTML = '';

    const parts = currentFolder === '/' ? [] : currentFolder.split('/').filter(Boolean);

    // Root segmen
    const rootSeg = document.createElement('span');
    rootSeg.className = 'breadcrumb-segment' + (parts.length === 0 ? ' active' : '');
    rootSeg.textContent = '📂 / (root)';
    rootSeg.dataset.path = '/';
    rootSeg.addEventListener('click', () => navigateToFolder('/'));
    container.appendChild(rootSeg);

    // Path segments
    parts.forEach((part, i) => {
        const sep = document.createElement('span');
        sep.className = 'breadcrumb-separator';
        sep.textContent = '›';
        container.appendChild(sep);

        const seg = document.createElement('span');
        const segPath = '/' + parts.slice(0, i + 1).join('/');
        seg.className = 'breadcrumb-segment' + (i === parts.length - 1 ? ' active' : '');
        seg.textContent = '📂 ' + part;
        seg.dataset.path = segPath;
        seg.addEventListener('click', () => navigateToFolder(segPath));
        container.appendChild(seg);
    });
}

function navigateToFolder(path) {
    currentFolder = path;
    loadBackups();
}

function openMoveModal(backup) {
    const modal = document.getElementById('move-backup-modal');
    modal.dataset.backupKey = backup.key;
    document.getElementById('move-backup-filename').textContent = `Moving: ${backup.filename}`;

    // Populate folder dropdown
    const select = document.getElementById('move-dest-folder');
    select.innerHTML = '';
    if (currentFolder !== '/') {
        select.appendChild(new Option('/ (Root)', '/'));
    }
    folderList.forEach(f => {
        const fPath = typeof f === 'string' ? f : f.path;
        const managedByOtherProfile = typeof f !== 'string'
            && f.managed && f.profileId !== backup.profileId;
        if (fPath && fPath !== currentFolder && !managedByOtherProfile) {
            const opt = document.createElement('option');
            opt.value = fPath;
            opt.textContent = fPath;
            select.appendChild(opt);
        }
    });

    const moveButton = document.getElementById('btn-confirm-move');
    if (select.options.length === 0) {
        const option = new Option('No other folders available', '');
        option.disabled = true;
        select.appendChild(option);
        moveButton.disabled = true;
    } else {
        moveButton.disabled = false;
    }

    modal.classList.remove('hidden');
}

function updateFolderDropdowns() {
    // Update the quick backup folder dropdown
    const quickSelect = document.getElementById('quick-backup-folder');
    if (quickSelect) {
        const currentVal = quickSelect.value;
        quickSelect.innerHTML = '<option value="/">/ (Root)</option>';
        folderList.forEach(f => {
            const fPath = typeof f === 'string' ? f : f.path;
            const isManagedProfileFolder = typeof f !== 'string' && f.managed;
            if (fPath && !isManagedProfileFolder) {
                const opt = document.createElement('option');
                opt.value = fPath;
                opt.textContent = fPath;
                quickSelect.appendChild(opt);
            }
        });
        if (currentVal && [...quickSelect.options].some((option) => option.value === currentVal)) {
            quickSelect.value = currentVal;
        } else {
            quickSelect.value = '/';
        }
    }
}

// ────────────────────────────────────────────────────────────────
// Restore Modal (simplified — no passphrase)
// ────────────────────────────────────────────────────────────────
function openRestoreModal(key, filename) {
    restoreTarget = { key, filename };
    restoreInProgress = false;
    restoreCancelRequested = false;
    document.getElementById('restore-dest').value = '';
    document.getElementById('restore-progress-wrap').classList.add('hidden');
    document.getElementById('restore-progress-msg').classList.add('hidden');
    document.getElementById('restore-modal').classList.remove('hidden');
}

function resetRestoreModal() {
    document.getElementById('restore-modal').classList.add('hidden');
    restoreInProgress = false;
    restoreCancelRequested = false;
    const cancelBtn = document.getElementById('btn-cancel-restore');
    cancelBtn.textContent = 'Cancel';
    cancelBtn.disabled = false;
    document.getElementById('btn-confirm-restore').disabled = false;
    document.getElementById('btn-pick-restore-dest').disabled = false;
    document.getElementById('restore-progress-wrap').classList.add('hidden');
    document.getElementById('restore-progress-msg').classList.add('hidden');
    restoreTarget = null;
}

// ────────────────────────────────────────────────────────────────
// File Explorer
// ────────────────────────────────────────────────────────────────
async function openFileExplorer(key, filename) {
    explorerTarget = { key, filename };
    selectedExplorerFiles = new Set();
    updateSelectedCount();

    const container = document.getElementById('file-tree-container');
    container.innerHTML = '<div class="empty-state"><p class="text-muted">Loading file tree…</p></div>';
    document.getElementById('file-explorer-modal').classList.remove('hidden');

    try {
        const manifest = await invoke('cmd_get_backup_manifest', { key });
        explorerManifest = manifest;
        const files = manifest.files || [];
        if (files.length === 0) {
            container.innerHTML = `
                <div class="empty-state">
                    <p style="font-size: 2.5rem; margin-bottom: 1rem;">📦</p>
                    <p class="text-muted">File browsing is unsupported for deduplicated backups.</p>
                    <p class="text-muted text-sm" style="margin-top: 0.5rem;">Please click <strong>Restore All</strong> to restore your files.</p>
                </div>
            `;
        } else if (files.length === 1 && !files[0].path.includes('/') && !files[0].path.includes('\\')) {
            // Single file backup - show simple info
            const f = files[0];
            container.innerHTML = `
                <div class="empty-state">
                    <p style="font-size: 2rem; margin-bottom: 0.5rem;">📄</p>
                    <p><strong>${escapeHtml(f.path)}</strong></p>
                    <p class="text-muted text-sm">${formatBytes(f.size)}</p>
                </div>
            `;
        } else {
            renderFileTree(files);
        }
    } catch (err) {
        container.innerHTML = `
            <div class="empty-state">
                <p style="font-size: 2.5rem; margin-bottom: 1rem;">📦</p>
                <p class="text-muted">This backup was created before file browsing was available.</p>
                <p class="text-muted text-sm" style="margin-top: 0.5rem;">You can still <strong>Restore All</strong> to download and decrypt the entire backup.</p>
            </div>
        `;
    }
}

function closeFileExplorer() {
    document.getElementById('file-explorer-modal').classList.add('hidden');
    explorerTarget = null;
    explorerManifest = null;
    selectedExplorerFiles = new Set();
}

function renderFileTree(files) {
    const container = document.getElementById('file-tree-container');
    if (!files || files.length === 0) {
        container.innerHTML = '<div class="empty-state"><p class="text-muted">No files in this backup.</p></div>';
        return;
    }

    // Build tree structure from flat paths
    const root = {};
    files.forEach(f => {
        const parts = f.path.replace(/\\/g, '/').split('/');
        let current = root;
        parts.forEach((part, i) => {
            if (!current[part]) {
                current[part] = i === parts.length - 1
                    ? { __file: true, __data: f }
                    : {};
            }
            current = current[part];
        });
    });

    container.innerHTML = '';
    const ul = buildTreeUl(root, '');
    container.appendChild(ul);
}

function buildTreeUl(node, prefix) {
    const ul = document.createElement('ul');
    ul.className = 'file-tree';

    const entries = Object.entries(node).filter(([k]) => !k.startsWith('__'));
    // Sort: folders first, then files
    entries.sort(([aKey, aVal], [bKey, bVal]) => {
        const aIsFile = aVal.__file;
        const bIsFile = bVal.__file;
        if (aIsFile && !bIsFile) return 1;
        if (!aIsFile && bIsFile) return -1;
        return aKey.localeCompare(bKey);
    });

    entries.forEach(([name, value]) => {
        const li = document.createElement('li');
        li.className = 'file-tree-item';

        const fullPath = prefix ? `${prefix}/${name}` : name;

        if (value.__file) {
            // File
            const label = document.createElement('label');
            label.className = 'file-entry';
            const cb = document.createElement('input');
            cb.type = 'checkbox';
            cb.dataset.path = fullPath;
            cb.addEventListener('change', () => {
                if (cb.checked) selectedExplorerFiles.add(fullPath);
                else selectedExplorerFiles.delete(fullPath);
                updateSelectedCount();
            });
            const icon = document.createElement('span');
            icon.className = 'file-icon';
            icon.textContent = '📄';
            const nameSpan = document.createElement('span');
            nameSpan.className = 'file-name';
            nameSpan.textContent = name;
            const sizeSpan = document.createElement('span');
            sizeSpan.className = 'file-size';
            sizeSpan.textContent = formatBytes(value.__data.size);

            label.appendChild(cb);
            label.appendChild(icon);
            label.appendChild(nameSpan);
            label.appendChild(sizeSpan);
            li.appendChild(label);
        } else {
            // Folder
            const header = document.createElement('div');
            header.className = 'folder-header';
            const chevron = document.createElement('span');
            chevron.className = 'folder-chevron';
            chevron.textContent = '▶';
            const icon = document.createElement('span');
            icon.className = 'folder-icon';
            icon.textContent = '📁';
            const nameSpan = document.createElement('span');
            nameSpan.className = 'folder-name';
            nameSpan.textContent = name;

            header.appendChild(chevron);
            header.appendChild(icon);
            header.appendChild(nameSpan);
            li.appendChild(header);

            const childUl = buildTreeUl(value, fullPath);
            childUl.classList.add('collapsed');
            li.appendChild(childUl);

            header.addEventListener('click', () => {
                childUl.classList.toggle('collapsed');
                chevron.textContent = childUl.classList.contains('collapsed') ? '▶' : '▼';
            });
        }

        ul.appendChild(li);
    });

    return ul;
}

function updateSelectedCount() {
    const btn = document.getElementById('btn-restore-selected');
    const count = selectedExplorerFiles.size;
    btn.textContent = `Restore Selected (${count})`;
    btn.disabled = count === 0;
}

function formatBytes(bytes) {
    if (!bytes || bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
}

// ────────────────────────────────────────────────────────────────
// Database Backups
// ────────────────────────────────────────────────────────────────

function currentDatabaseFingerprint() {
    const editId = document.getElementById('database-edit-id').value;
    const password = document.getElementById('database-password').value;
    return JSON.stringify({
        editId,
        connectionUrl: document.getElementById('database-connection-url').value.trim(),
        password: editId && password === '' ? '[stored]' : password,
        dumpExecutable: document.getElementById('database-dump-executable').value.trim(),
        clientExecutable: document.getElementById('database-client-executable').value.trim(),
    });
}

function invalidateDatabaseConnectionTest() {
    databaseConnectionResult = null;
    databaseConnectionFingerprint = null;
    document.getElementById('database-selection-section').classList.add('hidden');
    document.getElementById('database-schedule-section').classList.add('hidden');
    document.getElementById('btn-save-database').disabled = true;
    document.getElementById('database-save-help').textContent = 'Test the connection again after changing connection details.';
    const status = document.getElementById('database-test-status');
    status.dataset.state = '';
    status.textContent = 'Test the connection to continue.';
}

async function loadDatabaseTools({ force = false, selectedDump = null } = {}) {
    const select = document.getElementById('database-tool-bundle');
    const previousDump = selectedDump || document.getElementById('database-dump-executable').value;
    select.disabled = true;
    select.innerHTML = '<option value="">Searching this PC…</option>';
    try {
        if (force || databaseTools.length === 0) {
            databaseTools = await invoke('cmd_discover_database_tools');
        }
        select.innerHTML = '';
        databaseTools.forEach((tool) => {
            const option = document.createElement('option');
            option.value = tool.id;
            option.textContent = `${tool.label} · ${tool.version}`;
            option.title = tool.version;
            select.appendChild(option);
        });
        const matching = databaseTools.find((tool) => tool.dumpExecutable === previousDump);
        if (previousDump && !matching) {
            const custom = document.createElement('option');
            custom.value = 'custom';
            custom.textContent = 'Custom executable paths';
            select.appendChild(custom);
            select.value = 'custom';
        } else if (matching) {
            select.value = matching.id;
        } else if (databaseTools.length > 0) {
            select.value = databaseTools[0].id;
            applySelectedDatabaseTool({ preserveTestState: true });
        } else {
            const custom = document.createElement('option');
            custom.value = 'custom';
            custom.textContent = 'No tools found, enter paths manually';
            select.appendChild(custom);
            document.querySelector('.database-tool-details').open = true;
        }
        updateDatabaseUsersOption();
    } catch (error) {
        select.innerHTML = '<option value="custom">Enter executable paths manually</option>';
        document.querySelector('.database-tool-details').open = true;
        showToast('Database tool scan failed: ' + friendlyError(error), 'error');
    } finally {
        select.disabled = false;
    }
}

function selectedDatabaseTool() {
    const selectedId = document.getElementById('database-tool-bundle').value;
    return databaseTools.find((tool) => tool.id === selectedId) || null;
}

function applySelectedDatabaseTool({ preserveTestState = false } = {}) {
    const tool = selectedDatabaseTool();
    if (tool) {
        document.getElementById('database-dump-executable').value = tool.dumpExecutable;
        document.getElementById('database-client-executable').value = tool.clientExecutable;
    } else {
        document.querySelector('.database-tool-details').open = true;
    }
    updateDatabaseUsersOption();
    if (!preserveTestState) invalidateDatabaseConnectionTest();
}

function updateDatabaseUsersOption() {
    const checkbox = document.getElementById('database-include-users');
    const help = document.getElementById('database-users-help');
    const tool = selectedDatabaseTool();
    const supported = tool ? Boolean(tool.supportsUserGrants) : true;
    checkbox.disabled = !supported;
    if (!supported) checkbox.checked = false;
    help.textContent = supported
        ? 'Exports portable users and grants when the dump tool supports it.'
        : 'This tool does not support portable user and grant exports.';
}

async function openDatabaseSetup(profile = null) {
    const form = document.getElementById('database-form');
    form.reset();
    databaseConnectionResult = null;
    databaseConnectionFingerprint = null;
    databaseSelectedDatabases = new Set(profile?.databases || []);
    databaseSelectedTables = new Set(profile?.tables || []);
    document.getElementById('database-edit-id').value = profile?.id || '';
    document.getElementById('database-setup-title').textContent = profile ? 'Edit database backup' : 'Add database backup';
    document.getElementById('database-name').value = profile?.name || '';
    document.getElementById('database-connection-url').value = profile?.connectionUrl || 'mysql://root@127.0.0.1:3306';
    document.getElementById('database-password').value = '';
    document.getElementById('database-password-help').textContent = profile
        ? 'Leave blank to keep the password already protected by Windows.'
        : 'Leave blank only when the database account has no password.';
    document.getElementById('database-dump-executable').value = profile?.dumpExecutable || '';
    document.getElementById('database-client-executable').value = profile?.clientExecutable || '';
    document.getElementById('database-include-new').checked = profile ? Boolean(profile.includeNewDatabases) : true;
    document.getElementById('database-include-create').checked = profile ? Boolean(profile.includeCreateStatements) : true;
    document.getElementById('database-include-users').checked = Boolean(profile?.includeUsersAndGrants);
    document.querySelector(`input[name="database-scope"][value="${profile?.selectionMode || 'all'}"]`).checked = true;
    document.getElementById('database-schedule-times').value = '';
    document.getElementById('database-schedule-interval').value = 1;
    if (profile?.schedule) {
        try {
            const schedule = JSON.parse(profile.schedule);
            document.getElementById('database-schedule-times').value = (schedule.times || []).join(', ');
            document.getElementById('database-schedule-interval').value = schedule.intervalDays || 1;
        } catch {
            // Database profiles are created only with the current JSON schedule contract.
        }
    }
    document.getElementById('database-selection-section').classList.add('hidden');
    document.getElementById('database-schedule-section').classList.add('hidden');
    document.getElementById('btn-save-database').disabled = true;
    document.getElementById('database-save-help').textContent = 'A successful connection test is required.';
    const status = document.getElementById('database-test-status');
    status.dataset.state = '';
    status.textContent = 'Test the connection to continue.';
    document.getElementById('database-setup').classList.remove('hidden');
    await loadDatabaseTools({ selectedDump: profile?.dumpExecutable || null });
    if (profile?.dumpExecutable && !selectedDatabaseTool()) {
        document.getElementById('database-dump-executable').value = profile.dumpExecutable;
        document.getElementById('database-client-executable').value = profile.clientExecutable;
    }
    renderDatabaseScope();
    updateDatabaseSchedulePreview();
    document.getElementById('database-setup').scrollIntoView({ behavior: 'auto', block: 'start' });
}

function closeDatabaseSetup() {
    document.getElementById('database-setup').classList.add('hidden');
    databaseConnectionResult = null;
    databaseConnectionFingerprint = null;
    databaseSelectedDatabases = new Set();
    databaseSelectedTables = new Set();
}

function connectionPasswordPayload() {
    const editId = document.getElementById('database-edit-id').value;
    const value = document.getElementById('database-password').value;
    return editId && value === '' ? null : value;
}

async function testDatabaseConnection() {
    const button = document.getElementById('btn-test-database');
    const status = document.getElementById('database-test-status');
    const connectionUrl = document.getElementById('database-connection-url').value.trim();
    const dumpExecutable = document.getElementById('database-dump-executable').value.trim();
    const clientExecutable = document.getElementById('database-client-executable').value.trim();
    if (!connectionUrl || !dumpExecutable || !clientExecutable) {
        showToast('Enter a connection string and choose the database tools.', 'error');
        return;
    }
    button.disabled = true;
    button.textContent = 'Testing…';
    status.dataset.state = '';
    status.textContent = 'Connecting to the database…';
    try {
        databaseConnectionResult = await invoke('cmd_test_database_connection', {
            connectionUrl,
            password: connectionPasswordPayload(),
            dumpExecutable,
            clientExecutable,
            profileId: document.getElementById('database-edit-id').value || null,
        });
        databaseConnectionFingerprint = currentDatabaseFingerprint();
        status.dataset.state = 'success';
        status.textContent = `Connected to ${databaseConnectionResult.serverVersion}. ${databaseConnectionResult.databases.length} user database${databaseConnectionResult.databases.length === 1 ? '' : 's'} found.`;
        document.getElementById('database-selection-section').classList.remove('hidden');
        document.getElementById('database-schedule-section').classList.remove('hidden');
        document.getElementById('btn-save-database').disabled = false;
        document.getElementById('database-save-help').textContent = 'The connection is verified. Save when the scope and schedule look right.';
        renderDatabaseSelections();
    } catch (error) {
        databaseConnectionResult = null;
        databaseConnectionFingerprint = null;
        status.dataset.state = 'error';
        status.textContent = friendlyError(error);
        document.getElementById('database-selection-section').classList.add('hidden');
        document.getElementById('database-schedule-section').classList.add('hidden');
        document.getElementById('btn-save-database').disabled = true;
    } finally {
        button.disabled = false;
        button.textContent = 'Test Connection';
    }
}

function renderDatabaseSelections() {
    const databases = databaseConnectionResult?.databases || [];
    const databaseChecklist = document.getElementById('database-checklist');
    databaseChecklist.innerHTML = '';
    if (databases.length === 0) {
        databaseChecklist.innerHTML = '<p class="text-muted">No user databases were returned.</p>';
    } else {
        databases.forEach((database) => {
            const label = document.createElement('label');
            label.className = 'database-check-item';
            const checkbox = document.createElement('input');
            checkbox.type = 'checkbox';
            checkbox.value = database;
            checkbox.checked = databaseSelectedDatabases.has(database);
            checkbox.addEventListener('change', () => {
                if (checkbox.checked) databaseSelectedDatabases.add(database);
                else databaseSelectedDatabases.delete(database);
            });
            const name = document.createElement('span');
            name.textContent = database;
            name.title = database;
            label.append(checkbox, name);
            databaseChecklist.appendChild(label);
        });
    }

    const tableDatabase = document.getElementById('database-table-database');
    const previous = tableDatabase.value || [...databaseSelectedDatabases][0] || databases[0] || '';
    tableDatabase.innerHTML = '';
    databases.forEach((database) => {
        const option = document.createElement('option');
        option.value = database;
        option.textContent = database;
        tableDatabase.appendChild(option);
    });
    if (databases.includes(previous)) tableDatabase.value = previous;
    renderDatabaseScope();
}

function selectedDatabaseScope() {
    return document.querySelector('input[name="database-scope"]:checked')?.value || 'all';
}

function renderDatabaseScope() {
    const scope = selectedDatabaseScope();
    document.getElementById('database-all-options').classList.toggle('hidden', scope !== 'all');
    document.getElementById('database-database-options').classList.toggle('hidden', scope !== 'databases');
    document.getElementById('database-table-options').classList.toggle('hidden', scope !== 'tables');
}

async function loadDatabaseTables() {
    if (!databaseConnectionResult || databaseConnectionFingerprint !== currentDatabaseFingerprint()) {
        showToast('Test the connection again before loading tables.', 'error');
        return;
    }
    const database = document.getElementById('database-table-database').value;
    if (!database) {
        showToast('Choose a database first.', 'error');
        return;
    }
    const button = document.getElementById('btn-load-database-tables');
    const container = document.getElementById('database-table-checklist');
    button.disabled = true;
    button.textContent = 'Loading…';
    container.innerHTML = '<p class="text-muted">Loading tables…</p>';
    try {
        const tables = await invoke('cmd_list_database_tables', {
            connectionUrl: document.getElementById('database-connection-url').value.trim(),
            password: connectionPasswordPayload(),
            clientExecutable: document.getElementById('database-client-executable').value.trim(),
            profileId: document.getElementById('database-edit-id').value || null,
            database,
        });
        container.innerHTML = '';
        if (!tables.length) {
            container.innerHTML = '<p class="text-muted">No tables or views found.</p>';
            return;
        }
        tables.forEach((table) => {
            const label = document.createElement('label');
            label.className = 'database-check-item';
            const checkbox = document.createElement('input');
            checkbox.type = 'checkbox';
            checkbox.value = table;
            checkbox.checked = databaseSelectedTables.has(table);
            checkbox.addEventListener('change', () => {
                if (checkbox.checked) databaseSelectedTables.add(table);
                else databaseSelectedTables.delete(table);
            });
            const name = document.createElement('span');
            name.textContent = table;
            name.title = table;
            label.append(checkbox, name);
            container.appendChild(label);
        });
    } catch (error) {
        container.innerHTML = `<p class="text-muted">${escapeHtml(friendlyError(error))}</p>`;
    } finally {
        button.disabled = false;
        button.textContent = 'Load tables';
    }
}

function databaseScheduleValue() {
    const raw = document.getElementById('database-schedule-times').value.trim();
    if (!raw) return null;
    const times = raw.split(',').map((value) => value.trim()).filter(Boolean);
    const validTime = /^([01]\d|2[0-3]):([0-5]\d)$/;
    const invalid = times.find((value) => !validTime.test(value));
    if (invalid) throw new Error(`Invalid time "${invalid}". Use HH:MM in 24-hour time.`);
    const intervalDays = Math.max(1, Math.min(365, Math.trunc(Number(document.getElementById('database-schedule-interval').value || 1))));
    return JSON.stringify({ times, intervalDays });
}

function updateDatabaseSchedulePreview() {
    const help = document.getElementById('database-schedule-help');
    const raw = document.getElementById('database-schedule-times').value.trim();
    if (!raw) {
        help.textContent = 'Leave blank for a manual-only database backup.';
        return;
    }
    try {
        const schedule = JSON.parse(databaseScheduleValue());
        const utc = schedule.times.map((value) => {
            const [hours, minutes] = value.split(':').map(Number);
            const local = new Date();
            local.setHours(hours, minutes, 0, 0);
            return `${value} local (${String(local.getUTCHours()).padStart(2, '0')}:${String(local.getUTCMinutes()).padStart(2, '0')} UTC)`;
        });
        help.textContent = `${utc.join(', ')} · every ${schedule.intervalDays} day${schedule.intervalDays === 1 ? '' : 's'}`;
    } catch (error) {
        help.textContent = error.message;
    }
}

async function saveDatabaseProfile() {
    if (!databaseConnectionResult || databaseConnectionFingerprint !== currentDatabaseFingerprint()) {
        showToast('Test the current connection before saving.', 'error');
        invalidateDatabaseConnectionTest();
        return;
    }
    const name = document.getElementById('database-name').value.trim();
    if (!name) {
        showToast('Enter a name for this database backup.', 'error');
        return;
    }
    const selectionMode = selectedDatabaseScope();
    let databases = [];
    let tables = [];
    if (selectionMode === 'databases') {
        databases = [...databaseSelectedDatabases];
        if (databases.length === 0) {
            showToast('Select at least one database.', 'error');
            return;
        }
    } else if (selectionMode === 'tables') {
        const database = document.getElementById('database-table-database').value;
        tables = [...databaseSelectedTables];
        if (!database || tables.length === 0) {
            showToast('Choose a database and at least one table.', 'error');
            return;
        }
        databases = [database];
    } else if (!document.getElementById('database-include-new').checked) {
        databases = [...(databaseConnectionResult?.databases || [])];
        if (databases.length === 0) {
            showToast('No user databases are available to freeze in this backup scope.', 'error');
            return;
        }
    }
    let schedule;
    try {
        schedule = databaseScheduleValue();
    } catch (error) {
        showToast(error.message, 'error');
        return;
    }
    const editId = document.getElementById('database-edit-id').value;
    const payload = {
        name,
        connectionUrl: document.getElementById('database-connection-url').value.trim(),
        password: connectionPasswordPayload(),
        dumpExecutable: document.getElementById('database-dump-executable').value.trim(),
        clientExecutable: document.getElementById('database-client-executable').value.trim(),
        selectionMode,
        databases,
        tables,
        includeNewDatabases: selectionMode === 'all' && document.getElementById('database-include-new').checked,
        includeCreateStatements: document.getElementById('database-include-create').checked,
        includeUsersAndGrants: document.getElementById('database-include-users').checked,
        schedule,
    };
    const button = document.getElementById('btn-save-database');
    button.disabled = true;
    button.textContent = 'Saving…';
    try {
        if (editId) {
            await invoke('cmd_update_database_profile', { id: editId, ...payload, enabled: true });
            showToast('Database backup updated.', 'success');
        } else {
            await invoke('cmd_create_database_profile', { ...payload, password: payload.password ?? '' });
            showToast('Database backup created.', 'success');
        }
        closeDatabaseSetup();
        await loadDatabaseProfiles();
    } catch (error) {
        showToast(friendlyError(error), 'error');
        button.disabled = false;
    } finally {
        button.textContent = 'Save Database Backup';
    }
}

function databaseScheduleLabel(profile) {
    if (!profile.schedule) return 'Manual only';
    try {
        const schedule = JSON.parse(profile.schedule);
        const cadence = Number(schedule.intervalDays) === 1 ? 'Daily' : `Every ${schedule.intervalDays} days`;
        return `${(schedule.times || []).join(', ')} local · ${cadence}`;
    } catch {
        return 'Scheduled';
    }
}

function databaseScopeLabel(profile) {
    if (profile.selectionMode === 'all') {
        return profile.includeNewDatabases
            ? 'Every user database · new databases included'
            : `${profile.databases.length} verified database${profile.databases.length === 1 ? '' : 's'} · fixed scope`;
    }
    if (profile.selectionMode === 'tables') {
        return `${profile.tables.length} table${profile.tables.length === 1 ? '' : 's'} in ${profile.databases[0] || 'one database'}`;
    }
    return `${profile.databases.length} database${profile.databases.length === 1 ? '' : 's'}`;
}

function databaseConnectionLabel(connectionUrl) {
    try {
        const url = new URL(connectionUrl);
        return `${decodeURIComponent(url.username)}@${url.hostname}:${url.port || '3306'}`;
    } catch {
        return connectionUrl;
    }
}

async function loadDatabaseProfiles() {
    const container = document.getElementById('database-list');
    try {
        const [profiles, fileProfiles, profileLimitValue, backupState] = await Promise.all([
            invoke('cmd_list_database_profiles'),
            invoke('cmd_list_profiles'),
            invoke('cmd_get_profile_limit'),
            invoke('cmd_list_backups'),
        ]);
        databaseProfiles = profiles || [];
        const profileLimit = Number(profileLimitValue ?? 2);
        const scheduledDatabases = databaseProfiles.filter((profile) => profile.enabled && String(profile.schedule || '').trim()).length;
        const scheduledFiles = (fileProfiles || []).filter((profile) => profile.enabled && String(profile.schedule || '').trim()).length;
        document.getElementById('database-limit-summary').textContent = `${scheduledFiles + scheduledDatabases} of ${profileLimit} automated backup profiles in use across files and databases. Manual-only backups do not count.`;
        container.innerHTML = '';
        if (databaseProfiles.length === 0) {
            const empty = document.createElement('div');
            empty.className = 'empty-state';
            empty.innerHTML = '<h3>No database backups yet</h3><p class="text-muted">Connect XAMPP, MariaDB or MySQL, verify access, then choose what SaveState should protect.</p>';
            const add = document.createElement('button');
            add.className = 'btn btn-primary';
            add.textContent = 'Add Database';
            add.addEventListener('click', () => void openDatabaseSetup());
            empty.appendChild(add);
            container.appendChild(empty);
            return;
        }

        databaseProfiles.forEach((profile) => {
            const row = document.createElement('article');
            row.className = 'database-row';
            row.dataset.databaseProfileId = profile.id;
            const needsAttention = profile.scheduleState === 'needs_attention';
            const effectiveNextRun = profile.scheduleState === 'retrying' && profile.retryAt ? profile.retryAt : profile.nextRun;
            const restorePoints = (backupState?.backups || [])
                .filter((backup) => backup.backupKind === 'database' && backup.databaseProfileId === profile.id)
                .sort((left, right) => new Date(right.lastModified) - new Date(left.lastModified));
            const statusLabel = needsAttention ? 'Needs attention' : (profile.enabled ? 'Active' : 'Paused');
            const statusClass = needsAttention || !profile.enabled ? 'badge-neutral' : 'badge-success';
            row.innerHTML = `
                <div class="database-row-header">
                    <div class="database-row-title">
                        <svg width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24" aria-hidden="true"><ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v6c0 1.7 3.6 3 8 3s8-1.3 8-3V5"/><path d="M4 11v6c0 1.7 3.6 3 8 3s8-1.3 8-3v-6"/></svg>
                        <h3 title="${escapeHtml(profile.name)}">${escapeHtml(profile.name)}</h3>
                    </div>
                    <span class="badge ${statusClass}">${statusLabel}</span>
                </div>
                <div class="database-row-meta">
                    <div class="profile-meta-item">
                        <span class="meta-label">Connection</span>
                        <span class="meta-value" title="${escapeHtml(profile.connectionUrl)}">${escapeHtml(databaseConnectionLabel(profile.connectionUrl))}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">Scope</span>
                        <span class="meta-value">${escapeHtml(databaseScopeLabel(profile))}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">Folder</span>
                        <span class="meta-value" title="${escapeHtml(profile.folder || '/')}">${escapeHtml(profile.folder || '/')}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">Last run</span>
                        <span class="meta-value">${profile.lastRun ? new Date(profile.lastRun).toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : 'Never'}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">${profile.scheduleState === 'retrying' ? 'Retry' : 'Next run'}</span>
                        <span class="meta-value" title="${effectiveNextRun ? escapeHtml(formatLocalAndUtc(effectiveNextRun)) : escapeHtml(databaseScheduleLabel(profile))}">${effectiveNextRun ? escapeHtml(formatLocalAndUtc(effectiveNextRun)) : escapeHtml(databaseScheduleLabel(profile))}</span>
                    </div>
                </div>
                ${profile.lastErrorCode ? `<p class="field-help">Last issue: ${escapeHtml(profile.lastErrorCode.replaceAll('_', ' '))}</p>` : ''}
                <details class="database-restore-points">
                    <summary>${restorePoints.length} restore point${restorePoints.length === 1 ? '' : 's'}</summary>
                    <div class="database-restore-list"></div>
                </details>
                <div class="database-row-progress hidden">
                    <div class="database-row-progress-info">
                        <span class="database-progress-message">Starting…</span>
                        <span class="database-progress-percent">0%</span>
                    </div>
                    <div class="progress-bar"><div class="progress-bar-fill database-progress-fill" style="width:0%"></div></div>
                </div>
                <div class="database-row-actions"></div>
            `;
            const restoreList = row.querySelector('.database-restore-list');
            if (restorePoints.length === 0) {
                restoreList.innerHTML = '<p class="text-muted">Run the first backup to create a restore point.</p>';
            } else {
                restorePoints.forEach((backup) => {
                    const item = document.createElement('div');
                    item.className = 'database-restore-item';
                    const copy = document.createElement('div');
                    const date = document.createElement('strong');
                    date.textContent = new Date(backup.lastModified).toLocaleString();
                    const size = document.createElement('span');
                    size.textContent = `${backup.sizeFormatted} source SQL`;
                    copy.append(date, size);
                    const restore = document.createElement('button');
                    restore.className = 'btn btn-ghost btn-sm';
                    restore.textContent = 'Restore';
                    restore.addEventListener('click', async () => {
                        const confirmed = await confirmDialog(
                            `Import this restore point into ${databaseConnectionLabel(profile.connectionUrl)}? Existing objects may be replaced. A stopped or failed SQL import can leave partial database changes, so keep a current safety backup.`,
                            { title: 'Restore database', kind: 'warning' },
                        );
                        if (!confirmed) return;
                        restore.disabled = true;
                        restore.textContent = 'Restoring…';
                        const stop = document.createElement('button');
                        stop.className = 'btn btn-danger btn-sm';
                        stop.textContent = 'Stop';
                        stop.addEventListener('click', async () => {
                            stop.disabled = true;
                            stop.textContent = 'Stopping…';
                            try {
                                await invoke('cmd_cancel_restore', { key: backup.key });
                            } catch (error) {
                                showToast('Could not stop restore: ' + friendlyError(error), 'error');
                                stop.disabled = false;
                                stop.textContent = 'Stop';
                            }
                        });
                        item.appendChild(stop);
                        row.querySelector('.database-row-progress').classList.remove('hidden');
                        try {
                            await invoke('cmd_restore_database_backup', {
                                profileId: profile.id,
                                snapshotId: backup.key,
                            });
                        } catch (error) {
                            void handleRepositoryError(error);
                        } finally {
                            stop.remove();
                            restore.disabled = false;
                            restore.textContent = 'Restore';
                            row.querySelector('.database-row-progress').classList.add('hidden');
                        }
                    });
                    item.append(copy, restore);
                    restoreList.appendChild(item);
                });
            }
            const actions = row.querySelector('.database-row-actions');
            const run = document.createElement('button');
            run.className = 'btn btn-primary btn-sm';
            run.dataset.databaseAction = 'run';
            run.textContent = 'Run Now';
            run.addEventListener('click', () => {
                run.disabled = true;
                run.textContent = 'Running…';
                row.querySelector('.database-row-progress').classList.remove('hidden');
                invoke('cmd_run_database_backup', { profileId: profile.id }).catch((error) => {
                    row.querySelector('.database-row-progress').classList.add('hidden');
                    run.disabled = false;
                    run.textContent = 'Run Now';
                    failBackupUi(error);
                });
            });
            const edit = document.createElement('button');
            edit.className = 'btn btn-ghost btn-sm';
            edit.textContent = 'Edit';
            edit.addEventListener('click', () => void openDatabaseSetup(profile));
            const openBackups = document.createElement('button');
            openBackups.className = 'btn btn-ghost btn-sm';
            openBackups.textContent = 'Open Backups';
            openBackups.addEventListener('click', () => openManagedProfileFolder(profile.folder));
            const remove = document.createElement('button');
            remove.className = 'btn btn-danger btn-sm';
            remove.textContent = 'Delete';
            remove.addEventListener('click', () => openProfileDeleteModal(profile, 'database'));
            actions.append(run, edit, openBackups, remove);
            container.appendChild(row);
        });
    } catch (error) {
        container.innerHTML = `<div class="empty-state"><p class="text-muted">Could not load database backups: ${escapeHtml(friendlyError(error))}</p></div>`;
    }
}

// ────────────────────────────────────────────────────────────────
// Profiles
// ────────────────────────────────────────────────────────────────
function profileFolderName(name) {
    const normalized = String(name || '')
        .replaceAll('/', ' ')
        .replaceAll('\\', ' ')
        .replace(/[\u0000-\u001f\u007f]/g, ' ')
        .replace(/[^a-zA-Z0-9 _-]+/g, ' ')
        .replace(/\s+/g, ' ')
        .trim()
        .slice(0, 50)
        .trim();
    return normalized && normalized !== '.' && normalized !== '..' ? normalized : 'Backup Profile';
}

function updateProfileFolderPreview() {
    const preview = document.getElementById('profile-folder-preview');
    if (!preview) return;
    preview.textContent = `/${profileFolderName(document.getElementById('profile-name')?.value)}`;
}

function openManagedProfileFolder(folder) {
    currentFolder = folder || '/';
    navigateTo('backups');
}

function openProfileDeleteModal(profile, kind) {
    pendingProfileDeletion = {
        id: profile.id,
        name: profile.name,
        folder: profile.folder || '/',
        kind,
    };
    document.getElementById('profile-delete-title').textContent = `Delete "${profile.name}"?`;
    document.getElementById('profile-delete-copy').textContent = kind === 'database'
        ? `The database connection and saved password will be removed. Leave the option below unchecked to keep ${profile.folder || 'its backup folder'} and every restore point.`
        : `The schedule and profile settings will be removed. Leave the option below unchecked to keep ${profile.folder || 'its backup folder'} and every backup.`;
    document.getElementById('profile-delete-backups').checked = false;
    document.getElementById('profile-delete-modal').classList.remove('hidden');
}

function closeProfileDeleteModal() {
    pendingProfileDeletion = null;
    document.getElementById('profile-delete-modal').classList.add('hidden');
    document.getElementById('profile-delete-backups').checked = false;
}

async function loadProfiles() {
    const container = document.getElementById('profiles-list');
    try {
        const [profiles, unownedCount, authStatus, profileLimitValue] = await Promise.all([
            invoke('cmd_list_profiles'),
            invoke('cmd_count_unowned_profiles'),
            invoke('cmd_get_auth_status'),
            invoke('cmd_get_profile_limit'),
        ]);
        const profileLimit = Number(profileLimitValue ?? 2);
        const automatedCount = (profiles || []).filter((profile) => profile.enabled && String(profile.schedule || '').trim()).length;
        const profileLimitSummary = document.getElementById('profile-limit-summary');
        if (profileLimitSummary) {
            profileLimitSummary.textContent = `${automatedCount} of ${profileLimit} automated backup profiles in use. Manual-only profiles do not count.`;
        }
        container.innerHTML = '';

        if (Number(unownedCount) > 0) {
            const migrationCard = document.createElement('div');
            migrationCard.className = 'profile-card glass-card';
            migrationCard.innerHTML = `
                <div class="profile-card-header">
                    <h3 class="profile-name">Existing profiles need an owner</h3>
                    <span class="badge badge-neutral">Paused</span>
                </div>
                <p class="text-muted">
                    ${Number(unownedCount)} profile${Number(unownedCount) === 1 ? '' : 's'} from an earlier SaveState version are hidden and cannot run until you assign them to the correct account.
                    If these are not yours, sign out and use the account that created them.
                </p>
                <div class="profile-actions"></div>
            `;
            const claimButton = document.createElement('button');
            claimButton.className = 'btn btn-primary btn-sm';
            claimButton.textContent = 'Assign to this account';
            claimButton.addEventListener('click', async () => {
                const accountEmail = authStatus?.email || currentAccount?.email || 'this account';
                const confirmed = await confirmDialog(
                    `Assign all ${Number(unownedCount)} existing profile${Number(unownedCount) === 1 ? '' : 's'} on this Windows user to ${accountEmail}? Scheduled profiles within your allowance may become active immediately; any extras remain paused. Only continue if they belong to this account.`,
                    { title: 'Assign existing profiles', kind: 'warning' },
                );
                if (!confirmed) return;
                try {
                    const claimed = await invoke('cmd_claim_unowned_profiles');
                    showToast(`${claimed} profile${Number(claimed) === 1 ? '' : 's'} assigned to ${accountEmail}`, 'success');
                    loadProfiles();
                } catch (err) {
                    showToast(String(err), 'error');
                }
            });
            migrationCard.querySelector('.profile-actions').appendChild(claimButton);
            container.appendChild(migrationCard);
        }

        if (!profiles || profiles.length === 0) {
            const empty = document.createElement('div');
            empty.className = 'empty-state glass-card';
            empty.innerHTML = '<p class="text-muted">No backup profiles assigned to this account yet.</p>';
            container.appendChild(empty);
            return;
        }

        profiles.forEach(p => {
            const card = document.createElement('div');
            card.className = 'profile-card glass-card';
            card.setAttribute('data-profile-id', p.id);

            let scheduleLabel = 'Manual';
            if (p.schedule) {
                try {
                    const sched = JSON.parse(p.schedule);
                    const intervalDays = Number(sched.intervalDays || 0);
                    if (intervalDays > 0 && Array.isArray(sched.times) && sched.times.length > 0) {
                        const timesStr = sched.times.join(', ');
                        const daysStr = intervalDays === 1 ? 'Daily' : `Every ${intervalDays} days`;
                        scheduleLabel = `${timesStr} local - ${daysStr}`;
                    }
                } catch {
                    // Legacy format fallback
                    scheduleLabel = { hourly: 'Every hour', every_6h: 'Every 6h', daily: 'Daily', weekly: 'Weekly' }[p.schedule] || p.schedule;
                }
            }

            const effectiveNextRun = p.schedule_state === 'retrying' && p.retry_at ? p.retry_at : p.next_run;
            const nextRunLabel = effectiveNextRun ? formatLocalAndUtc(effectiveNextRun) : 'Manual only';

            card.innerHTML = `
                <div class="profile-card-header">
                    <h3 class="profile-name">${escapeHtml(p.name)}</h3>
                    <span class="badge ${p.enabled ? 'badge-success' : 'badge-neutral'}">${p.enabled ? 'Active' : 'Paused'}</span>
                </div>
                <div class="profile-meta">
                    <div class="profile-meta-item">
                        <span class="meta-label">Source</span>
                        <span class="meta-value" title="${escapeHtml(p.source_path)}">${escapeHtml(shortenPath(p.source_path))}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">Schedule</span>
                        <span class="meta-value" title="${escapeHtml(scheduleLabel)}">${escapeHtml(scheduleLabel)}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">Retention</span>
                        <span class="meta-value">${p.retention > 0 ? `Last ${p.retention}` : 'Unlimited'}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">Folder</span>
                        <span class="meta-value" title="${escapeHtml(p.folder || '/')}">${escapeHtml(p.folder || '/ (Root)')}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">Last Run</span>
                        <span class="meta-value">${p.last_run ? new Date(p.last_run).toLocaleString(undefined, {month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit'}) : 'Never'}</span>
                    </div>
                    <div class="profile-meta-item">
                        <span class="meta-label">${p.schedule_state === 'retrying' ? 'Retry' : 'Next Run'}</span>
                        <span class="meta-value" title="${escapeHtml(nextRunLabel)}">${escapeHtml(nextRunLabel)}</span>
                    </div>
                </div>
                <div class="profile-progress hidden">
                    <div class="profile-progress-info">
                        <span class="profile-progress-msg text-sm">Starting…</span>
                        <span class="profile-progress-pct text-sm font-bold">0%</span>
                    </div>
                    <div class="progress-bar" style="height: 4px;">
                        <div class="progress-bar-fill profile-progress-fill" style="width: 0%"></div>
                    </div>
                </div>
                <div class="profile-actions"></div>
            `;

            const actions = card.querySelector('.profile-actions');

            const runBtn = document.createElement('button');
            runBtn.className = 'btn btn-primary btn-sm';
            runBtn.textContent = '▶ Run Now';
            runBtn.addEventListener('click', () => {
                runBtn.disabled = true;
                runBtn.textContent = 'Running…';
                // Show inline progress bar on this card
                const progressWrap = card.querySelector('.profile-progress');
                if (progressWrap) progressWrap.classList.remove('hidden');

                showToast(`Backup started for "${p.name}"`, 'success');

                // Fire-and-forget — don't awai
                invoke('cmd_run_profile_backup', { profileId: p.id })
                    .then(() => {
                        // Backup finished — progress events already handle UI
                    })
                    .catch(err => {
                        failBackupUi(err);
                    });
            });

            const editBtn = document.createElement('button');
            editBtn.className = 'btn btn-ghost btn-sm';
            editBtn.textContent = '✏️ Edit';
            editBtn.addEventListener('click', () => openProfileModal(p));

            const openBackupsBtn = document.createElement('button');
            openBackupsBtn.className = 'btn btn-ghost btn-sm';
            openBackupsBtn.textContent = 'Open Backups';
            openBackupsBtn.addEventListener('click', () => openManagedProfileFolder(p.folder));

            const deleteBtn = document.createElement('button');
            deleteBtn.className = 'btn btn-danger btn-sm';
            deleteBtn.textContent = '🗑️ Delete';
            deleteBtn.addEventListener('click', () => openProfileDeleteModal(p, 'file'));

            actions.appendChild(runBtn);
            actions.appendChild(editBtn);
            actions.appendChild(openBackupsBtn);
            actions.appendChild(deleteBtn);
            container.appendChild(card);
        });
    } catch (err) {
        container.innerHTML = `<div class="empty-state glass-card"><p class="text-muted">Error loading profiles: ${escapeHtml(String(err))}</p></div>`;
    }
}

function openProfileModal(profile = null) {
    const modal = document.getElementById('profile-modal');
    const title = document.getElementById('profile-modal-title');
    const form = document.getElementById('profile-form');

    if (profile) {
        title.textContent = 'Edit Backup Profile';
        document.getElementById('profile-edit-id').value = profile.id;
        document.getElementById('profile-name').value = profile.name;
        document.getElementById('profile-source').value = profile.source_path;
        // Parse schedule
        if (profile.schedule) {
            try {
                const sched = JSON.parse(profile.schedule);
                document.getElementById('profile-schedule-times').value = (sched.times || []).join(', ');
                document.getElementById('profile-schedule-interval').value = sched.intervalDays || 0;
            } catch {
                // Legacy forma
                document.getElementById('profile-schedule-times').value = '';
                document.getElementById('profile-schedule-interval').value = 1;
            }
        } else {
            document.getElementById('profile-schedule-times').value = '';
            document.getElementById('profile-schedule-interval').value = 1;
        }
        document.getElementById('profile-retention').value = profile.retention || 0;
    } else {
        title.textContent = 'Create Backup Profile';
        form.reset();
        document.getElementById('profile-edit-id').value = '';
        document.getElementById('profile-schedule-interval').value = 1;
    }

    modal.classList.remove('hidden');
    updateProfileFolderPreview();
    updateScheduleTimePreview();
}

function updateScheduleTimePreview() {
    const intervalInput = document.getElementById('profile-schedule-interval');
    const timesInput = document.getElementById('profile-schedule-times');
    const help = document.getElementById('profile-schedule-time-help');
    if (!intervalInput || !timesInput || !help) return;

    const rawTimes = timesInput.value.split(',').map(value => value.trim()).filter(Boolean);
    if (rawTimes.length === 0) {
        help.textContent = 'Leave blank for a manual-only profile. UTC equivalents appear here after you enter a time.';
        return;
    }

    const validTime = /^([01]\d|2[0-3]):([0-5]\d)$/;
    if (rawTimes.some(value => !validTime.test(value))) {
        help.textContent = 'Use 24-hour HH:MM values separated by commas.';
        return;
    }

    const intervalDays = Math.max(1, Math.min(365, Math.trunc(Number(intervalInput.value || 1))));
    const now = new Date();
    const nextCandidates = rawTimes.map(value => {
        const [hour, minute] = value.split(':').map(Number);
        const localMidnight = new Date(now);
        localMidnight.setHours(0, 0, 0, 0);
        for (let occurrence = 0; occurrence <= 8; occurrence += 1) {
            const candidate = new Date(localMidnight);
            candidate.setDate(candidate.getDate() + (occurrence * intervalDays));
            candidate.setHours(hour, minute, 0, 0);
            // JavaScript normalizes a nonexistent spring-DST time (for
            // example 02:30) to a different wall-clock time. Skip that date,
            // matching the Rust scheduler instead of promising a false 03:30.
            if (candidate.getHours() !== hour || candidate.getMinutes() !== minute) continue;
            if (candidate > now) return candidate;
        }
        return null;
    }).filter(Boolean);
    if (nextCandidates.length === 0) {
        help.textContent = 'No valid local occurrence was found for these times.';
        return;
    }
    const next = new Date(Math.min(...nextCandidates.map(value => value.getTime())));
    const zone = Intl.DateTimeFormat().resolvedOptions().timeZone || 'machine local time';
    const local = next.toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
    const utc = next.toLocaleString(undefined, {
        timeZone: 'UTC',
        dateStyle: 'medium',
        timeStyle: 'short',
    });
    help.textContent = `Input uses ${zone}. Next occurrence: ${local} local / ${utc} UTC.`;
}

document.getElementById('profile-schedule-times')?.addEventListener('input', updateScheduleTimePreview);
document.getElementById('profile-schedule-interval')?.addEventListener('input', updateScheduleTimePreview);

function formatLocalAndUtc(value) {
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return 'Unknown';
    const local = date.toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
    const utc = date.toLocaleString(undefined, {
        timeZone: 'UTC',
        dateStyle: 'medium',
        timeStyle: 'short',
    });
    return `${local} local / ${utc} UTC`;
}

function shortenPath(path) {
    if (!path) return '';
    if (path.length <= 40) return path;
    const parts = path.replace(/\\/g, '/').split('/');
    if (parts.length <= 3) return path;
    return parts[0] + '/…/' + parts.slice(-2).join('/');
}

// ────────────────────────────────────────────────────────────────
// Settings — Notifications
// ────────────────────────────────────────────────────────────────
async function loadSettings() {
    void loadOrganizationInstallationStatus();
    try {
        const settings = await invoke('cmd_get_settings');
        const webhookInput = document.getElementById('settings-webhook-url');
        webhookInput.value = '';
        webhookInput.type = 'password';
        discordWebhookConfigured = settings.discordWebhookConfigured === true
            || Boolean(settings.discordWebhookUrl);
        updateWebhookStatus();

        const prefs = settings.notificationPrefs || {};
        document.getElementById('pref-backup-success').checked = (prefs.backupSuccess ?? prefs.backup_success) !== false;
        document.getElementById('pref-backup-failure').checked = (prefs.backupFailure ?? prefs.backup_failure) !== false;
        document.getElementById('pref-restore-success').checked = (prefs.restoreSuccess ?? prefs.restore_success) !== false;
        document.getElementById('pref-restore-failure').checked = (prefs.restoreFailure ?? prefs.restore_failure) !== false;
        document.getElementById('pref-backup-scheduled').checked = (prefs.backupScheduled ?? prefs.backup_scheduled) !== false;
    } catch {
        // Settings may not exist yet — that's OK
    }
}

function setOrganizationEnrollmentMessage(message, kind = 'error') {
    const error = document.getElementById('organization-enrollment-error');
    const warning = document.getElementById('organization-enrollment-warning');
    error.classList.add('hidden');
    warning.classList.add('hidden');
    if (!message) return;
    const target = kind === 'warning' ? warning : error;
    target.textContent = message;
    target.classList.remove('hidden');
}

function renderOrganizationInstallationStatus(status) {
    const connected = status?.connected === true;
    const statusText = document.getElementById('organization-installation-status');
    const openButton = document.getElementById('btn-open-organization-enrollment');
    if (connected) {
        statusText.textContent = `Connected to organization-managed storage for ${status.serverLabel || 'this server'}.`;
        openButton.classList.add('hidden');
        document.getElementById('organization-account-enrollments').classList.add('hidden');
        closeOrganizationEnrollment();
        return;
    }
    statusText.textContent = 'Checking this account for organization-managed storage…';
    openButton.classList.remove('hidden');
}

async function loadOrganizationInstallationStatus() {
    try {
        const status = await invoke('cmd_get_organization_installation_status');
        renderOrganizationInstallationStatus(status);
        if (!status?.connected) await loadAvailableOrganizationInstallations();
    } catch (error) {
        document.getElementById('organization-installation-status').textContent = 'Connection status is unavailable right now.';
        document.getElementById('btn-open-organization-enrollment').classList.remove('hidden');
        setOrganizationEnrollmentMessage(friendlyError(error));
    }
}

function renderAvailableOrganizationInstallations(installations) {
    organizationAvailableInstallations = Array.isArray(installations) ? installations : [];
    selectedOrganizationInstallationId = organizationAvailableInstallations[0]?.id || null;
    const section = document.getElementById('organization-account-enrollments');
    const list = document.getElementById('organization-account-installation-list');
    const status = document.getElementById('organization-installation-status');
    const connectButton = document.getElementById('btn-connect-account-organization');
    list.replaceChildren();

    if (organizationAvailableInstallations.length === 0) {
        section.classList.add('hidden');
        status.textContent = 'No organization installation is assigned to this account.';
        connectButton.disabled = true;
        return;
    }

    section.classList.remove('hidden');
    status.textContent = organizationAvailableInstallations.length === 1
        ? 'Organization-managed storage is ready to connect.'
        : `${organizationAvailableInstallations.length} organization installations are ready to connect.`;
    connectButton.disabled = false;

    organizationAvailableInstallations.forEach((installation, index) => {
        const label = document.createElement('label');
        label.className = 'organization-account-installation';

        const radio = document.createElement('input');
        radio.type = 'radio';
        radio.name = 'organization-account-installation';
        radio.value = installation.id;
        radio.checked = index === 0;
        radio.addEventListener('change', () => {
            selectedOrganizationInstallationId = radio.value;
        });

        const copy = document.createElement('span');
        copy.className = 'organization-account-installation-copy';
        const title = document.createElement('strong');
        title.textContent = installation.organizationName;
        const detail = document.createElement('span');
        detail.textContent = `${installation.customerName} · ${installation.serverLabel} · ${installation.platform}`;
        copy.append(title, detail);

        const quota = document.createElement('span');
        quota.className = 'organization-account-installation-quota';
        quota.textContent = `${formatBytes(installation.quotaBytes)} assigned`;
        label.append(radio, copy, quota);
        list.appendChild(label);
    });
}

async function loadAvailableOrganizationInstallations() {
    try {
        const result = await invoke('cmd_list_available_organization_installations');
        renderAvailableOrganizationInstallations(result.installations);
    } catch (error) {
        organizationAvailableInstallations = [];
        selectedOrganizationInstallationId = null;
        document.getElementById('organization-account-enrollments').classList.add('hidden');
        document.getElementById('organization-installation-status').textContent = 'Organization assignments could not be loaded.';
        setOrganizationEnrollmentMessage(friendlyError(error));
    }
}

async function connectAccountOrganizationInstallation() {
    if (!selectedOrganizationInstallationId) {
        setOrganizationEnrollmentMessage('Choose an organization installation for this PC.');
        return;
    }
    const button = document.getElementById('btn-connect-account-organization');
    setOrganizationEnrollmentMessage('');
    button.disabled = true;
    button.textContent = 'Connecting…';
    try {
        const result = await invoke('cmd_connect_organization_installation', {
            installationId: selectedOrganizationInstallationId,
        });
        renderOrganizationInstallationStatus(result);
        if (result.persistenceWarning) {
            setOrganizationEnrollmentMessage(result.persistenceWarning, 'warning');
        } else {
            showToast(`Organization installation connected for ${result.serverLabel}.`, 'success');
        }
        warmRepositoryInBackground();
    } catch (error) {
        setOrganizationEnrollmentMessage(friendlyError(error));
        await loadAvailableOrganizationInstallations();
    } finally {
        button.disabled = false;
        button.textContent = 'Connect this PC';
    }
}

function openOrganizationEnrollment() {
    const form = document.getElementById('organization-enrollment-form');
    const button = document.getElementById('btn-open-organization-enrollment');
    form.classList.remove('hidden');
    button.setAttribute('aria-expanded', 'true');
    setOrganizationEnrollmentMessage('');
    document.getElementById('organization-setup-token').focus();
}

function resetOrganizationEnrollmentPreview() {
    organizationEnrollmentPreview = null;
    document.getElementById('organization-enrollment-preview').classList.add('hidden');
    setOrganizationEnrollmentMessage('');
}

async function pasteOrganizationSetupToken() {
    const input = document.getElementById('organization-setup-token');
    try {
        input.value = (await navigator.clipboard.readText()).trim();
        resetOrganizationEnrollmentPreview();
        input.focus();
    } catch {
        setOrganizationEnrollmentMessage('SaveState could not read the clipboard. Paste the token into the field manually.');
    }
}

function closeOrganizationEnrollment() {
    organizationEnrollmentPreview = null;
    document.getElementById('organization-enrollment-form').classList.add('hidden');
    document.getElementById('organization-enrollment-preview').classList.add('hidden');
    document.getElementById('organization-setup-token').value = '';
    document.getElementById('btn-open-organization-enrollment').setAttribute('aria-expanded', 'false');
    setOrganizationEnrollmentMessage('');
}

function enrollmentDate(value) {
    if (!value) return 'Unknown';
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return 'Unknown';
    return parsed.toLocaleString();
}

async function reviewOrganizationEnrollment() {
    const token = document.getElementById('organization-setup-token').value.trim();
    const button = document.getElementById('btn-review-organization-enrollment');
    if (!token) {
        setOrganizationEnrollmentMessage('Paste the one-time setup token first.');
        return;
    }
    setOrganizationEnrollmentMessage('');
    button.disabled = true;
    button.textContent = 'Reviewing…';
    try {
        const result = await invoke('cmd_inspect_organization_installation', { token });
        organizationEnrollmentPreview = result.enrollment;
        document.getElementById('organization-preview-name').textContent = result.enrollment.organization.name;
        document.getElementById('organization-preview-customer').textContent = result.enrollment.customer.displayName;
        document.getElementById('organization-preview-server').textContent = result.enrollment.installation.serverLabel;
        document.getElementById('organization-preview-expiry').textContent = enrollmentDate(result.enrollment.expiresAt);
        document.getElementById('organization-enrollment-preview').classList.remove('hidden');
    } catch (error) {
        organizationEnrollmentPreview = null;
        document.getElementById('organization-enrollment-preview').classList.add('hidden');
        setOrganizationEnrollmentMessage(friendlyError(error));
    } finally {
        button.disabled = false;
        button.textContent = 'Review';
    }
}

async function confirmOrganizationEnrollment() {
    if (!organizationEnrollmentPreview) {
        setOrganizationEnrollmentMessage('Review the setup token before connecting.');
        return;
    }
    const token = document.getElementById('organization-setup-token').value.trim();
    const button = document.getElementById('btn-confirm-organization-enrollment');
    setOrganizationEnrollmentMessage('');
    button.disabled = true;
    button.textContent = 'Connecting…';
    try {
        const result = await invoke('cmd_redeem_organization_installation', { token });
        renderOrganizationInstallationStatus(result);
        if (result.persistenceWarning) {
            setOrganizationEnrollmentMessage(result.persistenceWarning, 'warning');
        } else {
            showToast(`Organization installation connected for ${result.serverLabel}.`, 'success');
        }
        warmRepositoryInBackground();
    } catch (error) {
        setOrganizationEnrollmentMessage(friendlyError(error));
    } finally {
        button.disabled = false;
        button.textContent = 'Connect installation';
    }
}

function readNotificationSettings(clearDiscordWebhook = false) {
    return {
        discordWebhookUrl: document.getElementById('settings-webhook-url').value.trim() || null,
        clearDiscordWebhook,
        notificationPrefs: {
            backupSuccess: document.getElementById('pref-backup-success').checked,
            backupFailure: document.getElementById('pref-backup-failure').checked,
            restoreSuccess: document.getElementById('pref-restore-success').checked,
            restoreFailure: document.getElementById('pref-restore-failure').checked,
            backupScheduled: document.getElementById('pref-backup-scheduled').checked,
        },
    };
}

async function saveSettings(showSuccess = true) {
    try {
        await invoke('cmd_save_settings', { settings: readNotificationSettings() });
        const webhookInput = document.getElementById('settings-webhook-url');
        if (webhookInput.value.trim()) {
            discordWebhookConfigured = true;
            webhookInput.value = '';
            updateWebhookStatus();
        }
        if (showSuccess) showToast('Notification settings saved.', 'success');
        return true;
    } catch (err) {
        showToast('Failed to save: ' + String(err), 'error');
        return false;
    }
}

function updateWebhookStatus() {
    const status = document.getElementById('settings-webhook-status');
    const input = document.getElementById('settings-webhook-url');
    status.textContent = discordWebhookConfigured
        ? 'A webhook is configured. Paste a new URL to replace it, or leave this field empty to keep it.'
        : 'No webhook is configured.';
    input.placeholder = discordWebhookConfigured
        ? 'Webhook configured — paste a new URL to replace it'
        : 'https://discord.com/api/webhooks/...';
}

function toggleWebhookVisibility() {
    const input = document.getElementById('settings-webhook-url');
    const button = document.getElementById('btn-toggle-webhook-visibility');
    const reveal = input.type === 'password';
    input.type = reveal ? 'url' : 'password';
    button.textContent = reveal ? 'Hide' : 'Show';
    button.setAttribute('aria-pressed', String(reveal));
}

async function removeWebhook() {
    if (!discordWebhookConfigured && !document.getElementById('settings-webhook-url').value.trim()) {
        showToast('No Discord webhook is configured.', 'info');
        return;
    }
    if (!window.confirm('Remove the saved Discord webhook? Notifications will stop until a new webhook is configured.')) return;
    try {
        await invoke('cmd_save_settings', { settings: readNotificationSettings(true) });
        discordWebhookConfigured = false;
        document.getElementById('settings-webhook-url').value = '';
        updateWebhookStatus();
        showToast('Discord webhook removed.', 'success');
    } catch (err) {
        showToast('Failed to remove webhook: ' + String(err), 'error');
    }
}

async function testNotification() {
    try {
        // A newly pasted URL is saved before testing. If the field is blank,
        // the API tests the existing write-only destination.
        if (!await saveSettings(false)) return;
        const result = await invoke('cmd_test_notification');
        if (result && result.sent) {
            const channels = (result.results || []).map(r => `${r.channel}: ${r.success ? 'OK' : 'FAILED'}`).join(', ');
            showToast(`Test notification sent! (${channels})`, 'success');
        } else if (result && !result.sent) {
            const failures = (result.results || [])
                .filter(channel => !channel.success)
                .map(channel => `${channel.channel}: ${channel.error || 'delivery failed'}`)
                .join(', ');
            showToast(failures || result.reason || 'No notification destination is configured.', 'error');
        } else {
            showToast('Discord did not confirm notification delivery.', 'error');
        }
    } catch (err) {
        showToast('Test failed: ' + String(err), 'error');
    }
}

// ────────────────────────────────────────────────────────────────
// Utilities
// ────────────────────────────────────────────────────────────────
function escapeHtml(str) {
    const div = document.createElement('div');
    div.textContent = str;
    return div.innerHTML;
}

function cssEscape(value) {
    if (window.CSS?.escape) return window.CSS.escape(String(value));
    return String(value).replace(/[^a-zA-Z0-9_-]/g, (character) => `\\${character}`);
}

// Make navigateTo globally available for inline onclick handlers
window.navigateTo = navigateTo;

// ────────────────────────────────────────────────────────────────
// Auto-Updater
// ────────────────────────────────────────────────────────────────
async function loadCurrentAppVersion() {
    try {
        currentAppVersion = await window.__TAURI__.app.getVersion();
    } catch (err) {
        currentAppVersion = 'Unknown';
        console.warn('[Updater] Could not read current app version:', err);
    }
    if (updatePhase === 'upToDate') {
        updateStatusMessage = `You're up to date${currentAppVersion ? ` on version ${currentAppVersion}` : ''}.`;
    }
    renderUpdaterUi();
}

function renderUpdaterUi() {
    const currentVersion = document.getElementById('settings-current-version');
    const settingsStatus = document.getElementById('settings-update-status');
    const settingsInstallButton = document.getElementById('btn-settings-install-update');
    const checkButton = document.getElementById('btn-check-updates');
    const bannerInstallButton = document.getElementById('btn-install-update');
    const bannerText = document.getElementById('update-banner-text');

    if (currentVersion) currentVersion.textContent = currentAppVersion || 'Loading…';
    if (settingsStatus) settingsStatus.textContent = updateStatusMessage;
    if (bannerText) bannerText.textContent = updateStatusMessage;

    const updateAvailable = Boolean(availableUpdateVersion);
    const busy = updateInstallInProgress || ['preparing', 'downloading', 'verifying'].includes(updatePhase);
    const installLabel = updatePhase === 'downloading'
        ? `Downloading${updateProgressPercent > 0 ? ` ${updateProgressPercent}%` : '…'}`
        : updatePhase === 'verifying'
            ? 'Installing…'
            : ['busy', 'installFailed'].includes(updatePhase)
                ? 'Try again'
                : updateAvailable
                    ? `Update to v${availableUpdateVersion}`
                    : 'Update';

    if (settingsInstallButton) {
        settingsInstallButton.classList.toggle('hidden', !updateAvailable);
        settingsInstallButton.disabled = busy;
        settingsInstallButton.textContent = installLabel;
    }
    if (bannerInstallButton) {
        bannerInstallButton.disabled = busy;
        bannerInstallButton.textContent = installLabel;
    }
    if (checkButton) {
        checkButton.disabled = updatePhase === 'checking' || busy;
        checkButton.textContent = updatePhase === 'checking' ? 'Checking…' : 'Check again';
    }
}

function showAvailableUpdateBanner(force = false) {
    if (!availableUpdateVersion) return;
    if (!force && (updateInstallInProgress || dismissedUpdateVersion === availableUpdateVersion)) return;
    document.getElementById('update-banner').classList.remove('hidden');
}

async function performUpdateCheck(revealAvailable) {
    const previouslyAvailableVersion = availableUpdateVersion;
    updatePhase = 'checking';
    updateStatusMessage = 'Checking for updates…';
    renderUpdaterUi();

    try {
        const { check } = window.__TAURI__.updater;
        const update = await check({ timeout: 30_000 });
        if (update) {
            availableUpdateVersion = update.version;
            await update.close();

            updatePhase = 'available';
            updateStatusMessage = `Version ${availableUpdateVersion} is ready. Updating waits until backups and restores are idle.`;
            showAvailableUpdateBanner(revealAvailable);
            console.log('[Updater] New version available:', availableUpdateVersion);
        } else {
            availableUpdateVersion = null;
            dismissedUpdateVersion = null;
            updatePhase = 'upToDate';
            updateStatusMessage = `You're up to date${currentAppVersion ? ` on version ${currentAppVersion}` : ''}.`;
            document.getElementById('update-banner').classList.add('hidden');
            console.log('[Updater] App is up to date.');
        }
    } catch (err) {
        if (previouslyAvailableVersion) {
            availableUpdateVersion = previouslyAvailableVersion;
            updatePhase = 'available';
            updateStatusMessage = `Version ${availableUpdateVersion} is ready. Updating waits until backups and restores are idle.`;
        } else {
            updatePhase = 'checkFailed';
            updateStatusMessage = 'Could not check for updates. Check your connection and try again.';
        }
        console.warn('[Updater] Check failed:', err);
    } finally {
        renderUpdaterUi();
    }
}

function checkForUpdates({ revealAvailable = false } = {}) {
    if (updateCheckPromise) return updateCheckPromise;

    updateCheckPromise = performUpdateCheck(revealAvailable)
        .finally(() => {
            updateCheckPromise = null;
        });
    return updateCheckPromise;
}

async function installUpdate() {
    if (!availableUpdateVersion || updateInstallInProgress) return;

    const dismissBtn = document.getElementById('btn-dismiss-update');
    const progressArea = document.getElementById('update-progress-area');
    const progressFill = document.getElementById('update-progress-fill');
    const progressPct = document.getElementById('update-progress-pct');
    updateInstallInProgress = true;
    dismissedUpdateVersion = null;
    updatePhase = 'preparing';
    updateStatusMessage = 'Checking that no backup, restore, or cleanup is running…';
    showAvailableUpdateBanner(true);
    dismissBtn.classList.add('hidden');
    progressArea.classList.remove('hidden');
    renderUpdaterUi();

    try {
        await invoke('cmd_install_update');

        // Windows exits when its installer starts. Relaunch platforms whose
        // updater returns after installation.
        const { relaunch } = window.__TAURI__.process;
        await relaunch();
    } catch (err) {
        console.error('[Updater] Install failed:', err);
        const message = String(err);
        if (message.includes('UPDATE_BUSY:')) {
            updatePhase = 'busy';
            updateStatusMessage = 'A backup, restore, or cleanup is running. Update after it finishes.';
        } else {
            updatePhase = 'installFailed';
            updateStatusMessage = 'The update could not be installed. Try again, or restart the app and retry.';
        }
        updateInstallInProgress = false;
        dismissBtn.classList.remove('hidden');
        progressArea.classList.add('hidden');
        progressFill.style.width = '0%';
        progressPct.textContent = '0%';
        updateProgressPercent = 0;
        renderUpdaterUi();
    }
}
