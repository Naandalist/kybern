import {expect,test} from 'bun:test'
import {loadKnownPulls as knownPulls, resolveKnownPull} from './knownPulls.ts'
import {resolvePull} from './tabTargets.ts'
import type {Project} from '../../../../packages/kybern-client/src/types.ts'

test('known-network discovery keeps partial/offline results and uses member remote identity even with no recent PRs',async()=>{
 const projects=[{id:'group',kind:'group',name:'Group',is_git:false},{id:'foreign',name:'Remote only',is_git:true,repo:{remote:'github.com/acme/remote'},space_id:'different-space'},{id:'offline',name:'Offline',is_git:true}] as Project[]
 const calls:{project:string;method:string}[]=[]
 const call=(async(project:string,method:string)=>{
   calls.push({project,method})
   if(project==='offline')throw Error('offline')
   if(method==='projects.members.status')return {members:[{path:'docs',name:'Docs',status:'ok',remote_url:'git@github.com:acme/docs.git'}]}
   return {pull_requests:[]}
 }) as Parameters<typeof knownPulls>[1]
 const data=await knownPulls(projects,call)
 expect(resolvePull('https://github.com/acme/docs/pull/999',data.repos)).toEqual({projectId:'group',repo:'docs',number:999})
 expect(resolvePull('https://github.com/acme/remote/pull/123',data.repos)).toEqual({projectId:'foreign',number:123})
 expect(data.errors).toEqual(["Offline: Couldn't load pull requests"])
 expect(calls).toContainEqual({project:'foreign',method:'github.pr.list'})
 expect(calls).toContainEqual({project:'group',method:'projects.members.status'})
})

test('identified transcript PR targets avoid a network-wide GitHub list scan',async()=>{
 const projects=[{id:'group',kind:'group',repo:{remote:'github.com/acme/api'},repos:[{remote:'github.com/acme/api'},{remote:'github.com/acme/docs'}]},{id:'foreign',is_git:true,repo:{remote:'github.com/acme/remote'}}] as Project[]
 const calls:string[]=[]
 const call=(async(project:string,method:string)=>{calls.push(`${project}:${method}`);return {members:[{path:'docs',status:'ok',remote_url:'git@github.com:acme/docs.git'}]}}) as Parameters<typeof knownPulls>[1]
 expect(await resolveKnownPull('https://github.com/acme/remote/pull/123',projects,call)).toEqual({projectId:'foreign',number:123})
 expect(calls).toEqual([])
 expect(await resolveKnownPull('https://github.com/acme/docs/pull/108',projects,call)).toEqual({projectId:'group',repo:'docs',number:108})
 expect(calls).toEqual(['group:projects.members.status'])
})

test('a failed project says what to do, never the raw daemon error', async () => {
  const { RpcCallError } = await import('../../../../packages/kybern-client/src/client.ts')
  const projects = [{ id: 'p', name: 'Kybern', is_git: true }] as Project[]
  const call = (async () => { throw new RpcCallError({ code: -32602, message: 'run gh' }) }) as Parameters<typeof knownPulls>[1]
  const data = await knownPulls(projects, call)
  expect(data.errors).toEqual(['Kybern: Install GitHub CLI to load pull requests'])
})
