// The saved records the machine lists leave out (ADE-124 Q26). Pure, so it is tested without a hub.

/**
 * Saved records that only repeat the machine the user is at: a non-member record of the same desktop profile as this
 * computer's own record, under an older id (an identity this Mac had before it joined a network). Its "Pair this device"
 * row was a second, unreachable "This Mac" (ADE-124 Q26), so the lists leave it out; nothing is deleted.
 */
export function staleSelfRecords(connections: readonly { record: { machineId: string; member: boolean; legacyProfileId?: string } }[], local: string | null | undefined): Set<string> {
  const profile = local ? connections.find((c) => c.record.machineId === local)?.record.legacyProfileId : undefined
  const stale = new Set<string>()
  if (!profile) return stale
  for (const { record } of connections) if (!record.member && record.machineId !== local && record.legacyProfileId === profile) stale.add(record.machineId)
  return stale
}
