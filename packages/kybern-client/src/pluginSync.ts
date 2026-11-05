import type { MachinePlugin, PluginsNetworkView, PluginsMachineView, PluginsCopyParams, PluginCopyJob } from './types.ts';

export const pluginKey = (p: Pick<MachinePlugin, 'provider' | 'id'>) => `${p.provider}:${p.id}`;
export const providerName = (p: MachinePlugin['provider']) => ({'claude-code': 'Claude Code', codex: 'Codex', opencode: 'OpenCode', pi: 'pi', omp: 'Oh My Pi', cursor: 'Cursor', antigravity: 'Antigravity', 'grok-build': 'Grok Build'}[p]);
export type PluginFilter = 'all' | 'different' | 'missing';
export type PluginCellState = 'same' | 'different' | 'missing' | 'unknown';
export interface PluginCell { machine: PluginsMachineView; plugin: MachinePlugin | null; state: PluginCellState }
export interface PluginRow { key: string; plugin: MachinePlugin; cells: PluginCell[]; different: boolean; missing: boolean }

/** Offline, denied and malformed inventories are unknown, never missing. Compare every declared setting. */
export function pluginRows(view: PluginsNetworkView, query = '', filter: PluginFilter = 'all'): PluginRow[] {
  const entries = new Map<string, MachinePlugin[]>();
  for (const machine of view.machines) for (const plugin of machine.inventory?.plugins ?? []) {
    const key = pluginKey(plugin); entries.set(key, [...entries.get(key) ?? [], plugin]);
  }
  return [...entries].map(([key, copies]) => {
    const frequency = new Map<string, number>(); for (const copy of copies) frequency.set(copy.fingerprint, (frequency.get(copy.fingerprint) ?? 0) + 1);
    const plugin = [...copies].sort((a, b) => (frequency.get(b.fingerprint) ?? 0) - (frequency.get(a.fingerprint) ?? 0))[0]!;
    const cells: PluginCell[] = view.machines.map(machine => {
      const found = machine.inventory?.plugins.find(p => pluginKey(p) === key) ?? null;
      const readable = plugin.enabled !== null && machine.state === 'reported' && !!machine.inventory && !machine.inventory.warnings.some(w => w.startsWith(plugin.provider));
      return {machine, plugin: found, state: !readable ? 'unknown' : !found ? 'missing' : found.fingerprint === plugin.fingerprint ? 'same' : 'different'};
    });
    return {key, plugin, cells, different: cells.some(c => c.state === 'different'), missing: cells.some(c => c.state === 'missing')};
  }).filter(r => (!query || `${r.plugin.name} ${providerName(r.plugin.provider)} ${r.plugin.source ?? ''}`.toLowerCase().includes(query.toLowerCase())) && (filter === 'all' || r[filter]))
    .sort((a, b) => a.key.localeCompare(b.key));
}
export function contentsText(p: MachinePlugin): string {
  const c = p.contents;
  const counts: [number, string][] = [[c.hooks, 'hook'], [c.mcp_servers, 'MCP server'], [c.commands, 'command'], [c.agents, 'agent'], [c.skills, 'skill'], [c.extensions, 'extension']];
  const parts = counts.filter(([n]) => n > 0).map(([n, name]) => `${n} ${name}${n === 1 ? '' : 's'}`);
  if (c.npm_packages.length) parts.push(`npm: ${c.npm_packages.join(', ')}`);
  return parts.join(' · ') || (p.kind === 'marketplace' ? 'Marketplace source' : 'Contents not fully declared');
}
export const enabledText = (p: MachinePlugin) => p.enabled === null ? 'State unknown' : p.enabled ? 'Enabled' : 'Disabled';
export function pluginCopyRequest(source: PluginsMachineView, plugin: MachinePlugin, targets: PluginsMachineView[], view: PluginsNetworkView, replace: boolean): PluginsCopyParams {
  return {provider: plugin.provider, id: plugin.id, from: source.is_self ? null : source.id, confirmed: plugin.fingerprint,
    to: targets.map(target => ({machine: target.id, replaces: replace ? view.machines.find(m => m.id === target.id)?.inventory?.plugins.find(p => pluginKey(p) === pluginKey(plugin))?.fingerprint ?? null : null}))};
}
export function jobText(job: PluginCopyJob): string {
  return job.result?.message ?? job.error ?? ({queued: 'Queued · installs when reachable', sending: 'Installing…', delivered: 'Delivered', failed: 'Failed', expired: 'Expired · retry from Machines', cancelled: 'Cancelled'}[job.state]);
}
