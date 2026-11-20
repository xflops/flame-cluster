import { test, expect } from '@playwright/test';

const liveURL = process.env.FLAME_E2E_URL;
const application = process.env.FLAME_E2E_APPLICATION;
const completedSession = process.env.FLAME_E2E_SESSION;
const openSession = process.env.FLAME_E2E_OPEN_SESSION;
const executorSession = process.env.FLAME_E2E_EXECUTOR_SESSION;
const worker = process.env.FLAME_E2E_WORKER;
const requests = new WeakMap();
const failures = new WeakMap();
const rowForSession = (page, id) => page.locator('#content tbody tr').filter({ has: page.getByRole('button', { name: id, exact: true }) });
const getResource = (response, resource) => response.request().method() === 'GET' && new URL(response.url()).pathname === `/api/${resource}`;

async function navigate(page, resource) {
  const loaded = page.waitForResponse(response => getResource(response, resource));
  await page.goto(`/#/${resource}`);
  const response = await loaded;
  expect(response.ok(), `${resource} HTTP response`).toBe(true);
  const data = await response.json();
  expect(data.items.length, `${resource} inventory must contain actual resources`).toBeGreaterThan(0);
  await expect(page.locator('#content tbody tr').first()).toBeVisible();
  return data;
}

async function refreshSessions(page) {
  const button = page.getByRole('button', { name: 'Refresh data' });
  await expect(button).toBeEnabled();
  const loaded = page.waitForResponse(response => getResource(response, 'sessions'));
  await button.click();
  expect((await loaded).ok(), 'session refresh HTTP response').toBe(true);
  await expect(button).toBeEnabled();
}

async function refreshSessionState(page, id, label) {
  await expect.poll(async () => {
    await refreshSessions(page);
    return rowForSession(page, id).locator('.badge').textContent();
  }, { timeout: 45_000, intervals: [500, 1000, 2000], message: `session ${id} reaches ${label} through the live console` }).toBe(label);
}

async function action(page, id, name, label) {
  const path = `/api/sessions/${encodeURIComponent(id)}/actions/${name}`;
  const changed = page.waitForResponse(response => response.request().method() === 'POST' && new URL(response.url()).pathname === path);
  const button = rowForSession(page, id).getByRole('button', { name: label, exact: true });
  await expect(button).toBeEnabled();
  await button.click();
  const response = await changed;
  expect(response.ok(), `${label} must succeed against the real FLM`).toBe(true);
}

test.describe('live Flame deployment', () => {
  test.skip(!liveURL, 'Set FLAME_E2E_URL and fixture identifiers to exercise a deployed FLM.');
  test.describe.configure({ mode: 'serial' });
  test.setTimeout(180_000);

  test.beforeAll(() => {
    for (const [name, value] of Object.entries({
      FLAME_E2E_APPLICATION: application,
      FLAME_E2E_SESSION: completedSession,
      FLAME_E2E_OPEN_SESSION: openSession,
      FLAME_E2E_EXECUTOR_SESSION: executorSession,
      FLAME_E2E_WORKER: worker,
    })) expect(value, `${name} is required for the live suite`).toBeTruthy();
    expect(openSession, 'mutation fixture must differ from the completed task session').not.toBe(completedSession);
  });

  test.beforeEach(async ({ page }) => {
    requests.set(page, []);
    failures.set(page, []);
    page.on('request', request => {
      const path = new URL(request.url()).pathname;
      if (path.startsWith('/api/')) requests.get(page).push(path);
    });
    page.on('response', response => {
      if (new URL(response.url()).pathname.startsWith('/api/') && !response.ok()) failures.get(page).push(`${response.status()} ${response.url()}`);
    });
  });

  test.afterEach(async ({ page }) => {
    const paths = requests.get(page);
    expect(paths.length, 'the browser must request live console APIs').toBeGreaterThan(0);
    expect(paths.every(path => /^\/api\/(?:connection|nodes|applications|sessions|executors)(?:\/[^/]+\/(?:tasks|actions\/(?:suspend|resume|close|delete)))?$/.test(path)), JSON.stringify(paths)).toBe(true);
    expect(paths.some(path => /\/(?:dashboard|clusters)(?:\/|$)/.test(path)), 'no legacy resource paths').toBe(false);
    expect(failures.get(page), 'all live browser API responses must succeed').toEqual([]);
    expect(new URL(page.url()).pathname, 'console is served at the root URL').toBe('/');
  });

  test('root console shows the registered application, real worker, and executor', async ({ page }) => {
    const applications = page.waitForResponse(response => getResource(response, 'applications'));
    await page.goto('/');
    expect((await applications).ok()).toBe(true);
    await expect(page).toHaveURL(/\/#\/applications$/);
    const applicationRow = page.locator('#content tbody tr').filter({ has: page.getByRole('button', { name: new RegExp(`^${application.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}(?:\\s|$)`) }) });
    await expect(applicationRow).toBeVisible();
    await expect(applicationRow.locator('.badge')).toHaveText('Enabled');

    const nodes = await navigate(page, 'nodes');
    const registered = nodes.items.find(node => node.metadata?.name === worker || node.spec?.hostname === worker);
    expect(registered, 'worker fixture is registered with FLM').toBeTruthy();
    const nodeRow = page.locator('#content tbody tr').filter({ hasText: worker });
    await expect(nodeRow).toBeVisible();
    await expect(nodeRow.locator('.readiness')).toHaveText('Ready');
    await nodeRow.getByRole('button').click();
    await expect(page.getByRole('dialog')).toContainText(worker);
    await expect(page.getByRole('dialog')).toContainText('Host');
    await page.getByRole('button', { name: 'Close details' }).click();

    const executors = await navigate(page, 'executors');
    const identities = new Set([worker, registered.metadata?.name, registered.metadata?.id].filter(Boolean));
    const executor = executors.items.find(executor =>
      identities.has(executor.spec?.node)
      && executor.spec?.application === application
      && executor.status?.session_id === executorSession
      && executor.status?.state === 4);
    expect(executor, 'the worker has a Bound executor for the active probe session and application').toBeTruthy();
    const executorID = executor.metadata?.id || executor.metadata?.name;
    expect(executorID, 'the active executor has an identity').toBeTruthy();
    const executorRow = page.locator('#content tbody tr').filter({ has: page.getByRole('button', { name: executorID, exact: true }) });
    await expect(executorRow).toBeVisible();
    await expect(executorRow.locator('.badge')).toHaveText('Bound');
    await expect(executorRow).toContainText(executorSession);
    await expect(executorRow.getByRole('cell', { name: application, exact: true })).toBeVisible();
    await expect(executorRow.getByRole('cell', { name: executor.spec.node, exact: true })).toBeVisible();
  });

  test('completed workload displays all four successful task records', async ({ page }) => {
    const sessions = await navigate(page, 'sessions');
    const session = sessions.items.find(row => (row.metadata?.id || row.metadata?.name) === completedSession);
    expect(session, 'completed fixture session is listed by the real FLM').toBeTruthy();
    expect(session.spec.application).toBe(application);
    expect(session.status.succeed).toBe(4);
    for (const phase of ['pending', 'running', 'failed', 'cancelled']) expect(session.status[phase] || 0).toBe(0);
    const row = rowForSession(page, completedSession);
    await expect(row).toContainText('4 / 4 completed');
    await expect(row.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '100');
    const taskPath = `/api/sessions/${encodeURIComponent(completedSession)}/tasks`;
    const loaded = page.waitForResponse(response => response.request().method() === 'GET' && new URL(response.url()).pathname === taskPath);
    await row.getByRole('button', { name: completedSession, exact: true }).click();
    const response = await loaded;
    expect(response.ok()).toBe(true);
    const tasks = await response.json();
    expect(tasks.total).toBe(4);
    expect(tasks.items).toHaveLength(4);
    expect(tasks.hasNext).toBe(false);
    expect(new Set(tasks.items.map(task => task.metadata.id)).size).toBe(4);
    for (const task of tasks.items) {
      expect(task.status.state, `task ${task.metadata.id} succeeded`).toBe(2);
      expect(Number(task.status.completion_time), `task ${task.metadata.id} completed`).toBeGreaterThan(0);
      await expect(page.locator('#tasks tbody').getByRole('cell', { name: task.metadata.id, exact: true })).toBeVisible();
    }
    await expect(page.locator('#tasks tbody tr')).toHaveCount(4);
    await expect(page.locator('#tasks tbody').getByText('Succeeded', { exact: true })).toHaveCount(4);
    await expect(page.getByRole('button', { name: 'Next tasks →' })).toBeDisabled();
  });

  test('session controls suspend, resume, close, and delete the live session', async ({ page }) => {
    await navigate(page, 'sessions');
    await page.getByRole('button', { name: 'Auto-refresh on', exact: true }).click();
    page.on('dialog', dialog => dialog.accept());
    await expect(rowForSession(page, openSession).locator('.badge')).toHaveText('Open');
    await action(page, openSession, 'suspend', 'Suspend');
    await refreshSessionState(page, openSession, 'Suspended');
    await action(page, openSession, 'resume', 'Resume');
    await refreshSessionState(page, openSession, 'Open');
    await action(page, openSession, 'close', 'Close');
    await refreshSessionState(page, openSession, 'Closed');
    await action(page, openSession, 'delete', 'Delete');
    await expect.poll(async () => {
      await refreshSessions(page);
      return rowForSession(page, openSession).count();
    }, { timeout: 45_000, intervals: [500, 1000, 2000] }).toBe(0);
    const response = await page.request.get('/api/sessions?page=1&pageSize=100');
    expect(response.ok()).toBe(true);
    const sessions = await response.json();
    expect(sessions.items.some(session => (session.metadata?.id || session.metadata?.name) === openSession), 'deleted session is absent from FLM, not just the DOM').toBe(false);
    expect(sessions.items.some(session => (session.metadata?.id || session.metadata?.name) === completedSession), 'unrelated completed workload remains').toBe(true);
  });
});
