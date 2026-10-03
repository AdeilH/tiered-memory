/**
 * Zero-dependency HTTP client for tiered-memory (Node 18+, fetch built in).
 *
 *   import { TieredMemory } from './tiered-memory.mjs';
 *   const mem = new TieredMemory('http://127.0.0.1:7900');
 *   const { params } = await mem.params({ user: 'adeel', project_id: 'teacher',
 *                                         defaults: { difficulty: 0.5 } });
 */

export class TieredMemoryError extends Error {
  constructor(status, message) {
    super(`tiered-memory ${status}: ${message}`);
    this.status = status;
  }
}

export class TieredMemory {
  constructor(baseUrl = 'http://127.0.0.1:7900', token = process.env.TM_TOKEN) {
    this.baseUrl = baseUrl.replace(/\/$/, '');
    this.token = token || null;
  }

  async #call(method, path, body) {
    const headers = {};
    if (body !== undefined) headers['content-type'] = 'application/json';
    if (this.token) headers['authorization'] = `Bearer ${this.token}`;
    const res = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const text = await res.text();
    const data = text ? JSON.parse(text) : null;
    if (!res.ok) {
      throw new TieredMemoryError(res.status, data?.error ?? text);
    }
    return data;
  }

  health() {
    return this.#call('GET', '/v1/health');
  }

  stats(user) {
    return this.#call('GET', `/v1/stats/${encodeURIComponent(user)}`);
  }

  registerProject(input) {
    return this.#call('POST', '/v1/projects', input);
  }

  listProjects(user) {
    return this.#call('GET', `/v1/projects/${encodeURIComponent(user)}`);
  }

  remember(input) {
    return this.#call('POST', '/v1/remember', input);
  }

  recall(input) {
    return this.#call('POST', '/v1/recall', input);
  }

  /** Returns { params, detail }: defaults merged under learner adjustments. */
  params(input) {
    return this.#call('POST', '/v1/params', input);
  }

  /** Assert one learner parameter (project-scoped unless global: true). */
  feedback({ user, key, value, project_id, weight, global }) {
    return this.#call('POST', '/v1/feedback', { user, key, value, project_id, weight, global });
  }

  consolidate(user) {
    return this.#call('POST', '/v1/consolidate', { user });
  }

  forget(input) {
    return this.#call('POST', '/v1/forget', input);
  }

  reindex(user) {
    return this.#call('POST', '/v1/reindex', { user });
  }
}
