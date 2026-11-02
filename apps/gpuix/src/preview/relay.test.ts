import { test, expect } from 'bun:test'
import { createServer, createConnection } from 'node:net'
import { bodyFraming, openRelay, rewriteRelayHead } from './relay.ts'

test('relay keeps the page credentials, rejects foreign origins and authority-form targets', () => {
  const head = rewriteRelayHead('POST /api?x=1 HTTP/1.1\r\nHost: 127.0.0.1:5321\r\nCookie: page=1\r\nAuthorization: Bearer app\r\nConnection: keep-alive\r\nContent-Length: 5\r\n\r\n', '127.0.0.1:4237', '/preview-proxy/T1', 5321)
  expect(head).toContain('POST /preview-proxy/T1/api?x=1 HTTP/1.1\r\nHost: 127.0.0.1:4237')
  expect(head).toContain('cookie: page=1'); expect(head).toContain('authorization: Bearer app'); expect(head).toContain('Connection: keep-alive')
  expect(head.match(/connection:/gi)).toHaveLength(1)
  for (const request of ['CONNECT example.com:443', 'GET http://evil/', 'GET //evil/']) expect(() => rewriteRelayHead(`${request} HTTP/1.1\r\nHost: 127.0.0.1:5321\r\n\r\n`, 'daemon:9', '/preview-proxy/T1', 5321)).toThrow()
  for (const host of ['evil.example:5321', '127.0.0.1:5322', '127.0.0.1:5321\r\nHost: evil']) expect(() => rewriteRelayHead(`GET / HTTP/1.1\r\nHost: ${host}\r\n\r\n`, 'daemon:9', '/preview-proxy/T1', 5321)).toThrow('421')
})

test('relay only accepts the configured daemon ticket endpoint', async () => {
  for (const upstream of ['http://evil/preview-proxy/T1', 'http://127.0.0.1:9/not-a-ticket', 'http://u:p@127.0.0.1:9/preview-proxy/T1', 'http://127.0.0.1:9/preview-proxy/../T1']) await expect(openRelay(upstream, 'http://127.0.0.1:9')).rejects.toThrow('configured daemon')
})

test('a page on another machine goes through the machine you are paired with, which gets its own credential and never the page', async () => {
  const head = rewriteRelayHead('GET /src/main.ts HTTP/1.1\r\nHost: 127.0.0.1:5321\r\nX-Kybern-Relay-Auth: Bearer page-made\r\nCookie: a=1\r\n\r\n', '100.64.0.2:4173', '/relay/m-1/http/preview-proxy/T1', 5321, [['x-kybern-relay-auth', 'Bearer device']])
  expect(head).toStartWith('GET /relay/m-1/http/preview-proxy/T1/src/main.ts HTTP/1.1\r\nHost: 100.64.0.2:4173')
  expect(head).toContain('x-kybern-relay-auth: Bearer device'); expect(head).not.toContain('page-made'); expect(head).toContain('cookie: a=1')
  let seen = ''
  const upstream = createServer(socket => { socket.once('data', data => { seen = data.toString(); socket.end('HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok') }) })
  await new Promise<void>(resolve => upstream.listen(0, '127.0.0.1', resolve))
  const daemon = `http://127.0.0.1:${(upstream.address() as { port: number }).port}`
  const relay = await openRelay(`${daemon}/relay/m-1/http/preview-proxy/T1/`, daemon, { relayAuth: 'device' })
  const port = Number(new URL(relay.url).port), socket = createConnection(port, '127.0.0.1')
  try {
    const reply = new Promise<string>((resolve, reject) => { let text = ''; socket.on('error', reject); socket.on('data', data => { text += data.toString() }); socket.on('close', () => resolve(text)) })
    socket.write(`GET /hmr HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\n\r\n`)
    expect(await reply).toContain('200 OK')
    expect(seen).toStartWith('GET /relay/m-1/http/preview-proxy/T1/hmr HTTP/1.1'); expect(seen).toContain('x-kybern-relay-auth: Bearer device')
  } finally { socket.destroy(); relay.close(); await new Promise<void>(resolve => upstream.close(() => resolve())) }
  // The machine path is only for a ticket route, and a daemon ticket never carries the credential.
  for (const upstream of ['http://127.0.0.1:9/relay/m-1/http/other', 'http://127.0.0.1:9/relay/m-1/ws', 'http://127.0.0.1:9/relay/../http/preview-proxy/T1']) await expect(openRelay(upstream, 'http://127.0.0.1:9', { relayAuth: 'device' })).rejects.toThrow('configured daemon')
})

test('relay bounds a complete request head as well as fragmented input', () => {
  expect(() => rewriteRelayHead(`GET / HTTP/1.1\r\nHost: localhost:5321\r\nX-Pad: ${'x'.repeat(65536)}\r\n\r\n`, 'daemon:9', '/preview-proxy/T1', 5321)).toThrow('431')
})

test('root assets and websocket bytes reach the ticket; closing destroys both connections', async () => {
  let seen = ''
  const upstream = createServer(socket => {
    socket.once('data', data => {
      seen = data.toString(); socket.write('HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n')
      socket.on('data', chunk => socket.write(chunk))
    })
  })
  await new Promise<void>(resolve => upstream.listen(0, '127.0.0.1', resolve))
  const daemon = `http://127.0.0.1:${(upstream.address() as { port: number }).port}`
  const relay = await openRelay(`${daemon}/preview-proxy/T1/`, daemon)
  const port = Number(new URL(relay.url).port), socket = createConnection(port, '127.0.0.1')
  try {
    let response = ''
    const echo = new Promise<void>((resolve, reject) => { socket.on('error', reject); socket.on('data', data => { response += data.toString(); if (response.endsWith('pong')) resolve(); else if (response.includes('\r\n\r\n')) socket.write('pong') }) })
    socket.write(`GET /src/main.ts?token=1 HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n`)
    await echo
    expect(seen).toStartWith('GET /preview-proxy/T1/src/main.ts?token=1 HTTP/1.1')
    expect(seen).not.toContain('authorization'); expect(response).toContain('101 Switching Protocols'); expect(response).toEndWith('pong')
  } finally { socket.destroy(); relay.close(); await new Promise<void>(resolve => upstream.close(() => resolve())) }
})

test('relay rejects traversal that would leave the authorized ticket path', () => {
  for (const path of ['/../api', '/%2e%2e/api', '/a/%2e%2e/api', '/%5c..%5capi', '/%00', '/%0d%0aHost:evil']) {
    expect(() => rewriteRelayHead(`GET ${path} HTTP/1.1\r\nHost: localhost:5321\r\n\r\n`, 'daemon:9', '/preview-proxy/T1', 5321)).toThrow('400')
  }
})

test('HTTPS relay validates trust and hostname before forwarding HTTP, assets, credentials and WebSocket bytes', async () => {
  const { mkdtempSync, readFileSync, rmSync, writeFileSync } = await import('node:fs')
  const { tmpdir } = await import('node:os')
  const { join } = await import('node:path')
  const { execFileSync } = await import('node:child_process')
  const { createServer: createTLS } = await import('node:tls')
  const dir = mkdtempSync(join(tmpdir(), 'gpuix-scratch-relay-tls-'))
  const openssl = (args: string[]) => execFileSync('openssl', args, { cwd: dir, stdio: 'ignore' })
  try {
    openssl(['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', 'ca.key', '-out', 'ca.pem', '-subj', '/CN=Kybern preview test CA', '-days', '1'])
    openssl(['req', '-newkey', 'rsa:2048', '-nodes', '-keyout', 'server.key', '-out', 'server.csr', '-subj', '/CN=localhost'])
    writeFileSync(join(dir, 'ext.cnf'), 'subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
    openssl(['x509', '-req', '-in', 'server.csr', '-CA', 'ca.pem', '-CAkey', 'ca.key', '-CAcreateserial', '-out', 'server.pem', '-days', '1', '-extfile', 'ext.cnf'])
    const seen: string[] = [], peers = new Set<import('node:tls').TLSSocket>()
    const upstream = createTLS({ key: readFileSync(join(dir, 'server.key')), cert: readFileSync(join(dir, 'server.pem')) }, socket => {
      peers.add(socket); socket.on('close', () => peers.delete(socket)); socket.on('error', () => {})
      socket.once('data', data => {
        seen.push(data.toString())
        if (data.includes('Upgrade: websocket') || data.includes('upgrade: websocket')) {
          socket.write('HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n'); socket.on('data', chunk => socket.write(chunk))
        } else socket.end('HTTP/1.1 200 OK\r\nContent-Length: 5\r\nSet-Cookie: session=page; HttpOnly\r\nConnection: close\r\n\r\nasset')
      })
    })
    upstream.on('tlsClientError', () => {})
    await new Promise<void>(resolve => upstream.listen(0, resolve))
    const port = (upstream.address() as { port: number }).port, daemon = `https://localhost:${port}`, ca = readFileSync(join(dir, 'ca.pem'))
    const relays: Awaited<ReturnType<typeof openRelay>>[] = []
    const request = async (host: string, trust?: { ca: Buffer }, websocket = false) => {
      const relay = await openRelay(`${host}/preview-proxy/secure/`, host, trust); relays.push(relay)
      const socket = createConnection(Number(new URL(relay.url).port), '127.0.0.1')
      return await new Promise<string>((resolve, reject) => {
        let result = '', pinged = false
        socket.on('error', reject); socket.setTimeout(3000, () => { socket.destroy(); reject(Error('test timeout')) })
        socket.on('data', data => { result += data.toString(); if (websocket && result.includes('101') && !pinged) { pinged = true; socket.write('tls-echo') } if (result.endsWith('tls-echo')) socket.end() })
        socket.on('close', () => resolve(result))
        socket.write(`GET /root/asset.js?q=1 HTTP/1.1\r\nHost: ${new URL(relay.url).host}\r\nCookie: session=page\r\nAuthorization: Bearer page-credential\r\n${websocket ? 'Connection: Upgrade\r\nUpgrade: websocket\r\n' : ''}\r\n`)
      })
    }
    try {
      const good = await request(daemon, { ca })
      expect(good).toContain('200 OK'); expect(good).toContain('Set-Cookie: session=page'); expect(good).toEndWith('asset')
      expect(seen[0]).toStartWith(`GET /preview-proxy/secure/root/asset.js?q=1 HTTP/1.1\r\nHost: localhost:${port}`)
      expect(seen[0]).toContain('cookie: session=page'); expect(seen[0]).toContain('authorization: Bearer page-credential')
      const ws = await request(daemon, { ca }, true)
      expect(ws).toContain('101 Switching Protocols'); expect(ws).toEndWith('tls-echo')
      const before = seen.length
      expect(await request(daemon)).toContain('502 Bad Gateway')
      expect(await request(`https://127.0.0.1:${port}`, { ca })).toContain('502 Bad Gateway')
      expect(seen.length).toBe(before)
    } finally { relays.forEach(relay => relay.close()); peers.forEach(socket => socket.destroy()); await new Promise<void>(resolve => upstream.close(() => resolve())) }
  } finally { rmSync(dir, { recursive: true, force: true }) }
}, 15000)

test('a single-page ticket (artifact preview, visual frame) is reachable only at the root of its origin (ADE-109)', () => {
  const head = (line: string) => `${line} HTTP/1.1\r\nHost: 127.0.0.1:5321\r\n\r\n`
  expect(rewriteRelayHead(head('GET /'), '100.64.0.2:4190', '/relay/m-1/http/visual-frame/V1', 5321, [['x-kybern-relay-auth', 'Bearer device']], true)).toStartWith('GET /relay/m-1/http/visual-frame/V1 HTTP/1.1\r\nHost: 100.64.0.2:4190')
  expect(rewriteRelayHead(head('GET /?a=1'), 'daemon:9', '/artifact-preview/A1', 5321, [], true)).toStartWith('GET /artifact-preview/A1?a=1 HTTP/1.1')
  for (const line of ['GET /favicon.ico', 'GET /assets/x.js', 'GET //']) expect(() => rewriteRelayHead(head(line), 'daemon:9', '/visual-frame/V1', 5321, [], true)).toThrow(/^40[45]/)
  expect(() => rewriteRelayHead('GET / HTTP/1.1\r\nHost: 127.0.0.1:5321\r\nUpgrade: websocket\r\n\r\n', 'daemon:9', '/visual-frame/V1', 5321, [], true)).toThrow('404')
})

test('the relay opens single-page tickets on the daemon or through a member, and nothing beside them', async () => {
  for (const path of ['/visual-frame/V1', '/artifact-preview/A1', '/relay/m-1/http/visual-frame/V1', '/relay/m-1/http/artifact-preview/A1']) (await openRelay(`http://127.0.0.1:9${path}`, 'http://127.0.0.1:9')).close()
  for (const path of ['/visual-frame/V1/', '/visual-frame/V1/x', '/artifact-preview/', '/assets/a1', '/relay/m-1/http/assets/a1']) await expect(openRelay(`http://127.0.0.1:9${path}`, 'http://127.0.0.1:9')).rejects.toThrow('configured daemon')
})

test('request bodies are framed so the next request on a kept-alive connection starts where the body ends', () => {
  const sized = bodyFraming('POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n')
  expect(sized.feed(Buffer.from('abc'))).toBe(3); expect(sized.done).toBe(false)
  expect(sized.feed(Buffer.from('deGET /'))).toBe(2); expect(sized.done).toBe(true)
  expect(bodyFraming('GET / HTTP/1.1\r\n\r\n').done).toBe(true)
  const chunked = bodyFraming('POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n')
  const wire = Buffer.from('4;ext=1\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: 1\r\n\r\nGET /next')
  let taken = 0
  for (let i = 0; i < wire.length && !chunked.done; i += 3) taken += chunked.feed(wire.subarray(i, Math.min(i + 3, wire.length)))
  expect(chunked.done).toBe(true)
  expect(wire.subarray(taken).toString()).toEndWith('GET /next')
  expect(wire.subarray(0, taken).toString()).toEndWith('X-Trailer: 1\r\n\r\n')
})

test('a page connection keeps one upstream connection for its requests', async () => {
  const seen: string[] = []
  let connections = 0
  const upstream = createServer(socket => {
    connections++
    let buffer = ''
    socket.on('data', data => {
      buffer += data.toString()
      for (;;) {
        const end = buffer.indexOf('\r\n\r\n'); if (end < 0) return
        const head = buffer.slice(0, end), length = Number(/content-length: (\d+)/i.exec(head)?.[1] ?? 0)
        if (buffer.length < end + 4 + length) return
        const body = buffer.slice(end + 4, end + 4 + length); buffer = buffer.slice(end + 4 + length)
        seen.push(`${head.split('\r\n')[0]}|${body}`)
        socket.write(`HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok`)
      }
    })
  })
  await new Promise<void>(resolve => upstream.listen(0, '127.0.0.1', resolve))
  const daemon = `http://127.0.0.1:${(upstream.address() as { port: number }).port}`
  const relay = await openRelay(`${daemon}/preview-proxy/T1/`, daemon)
  const port = Number(new URL(relay.url).port), socket = createConnection(port, '127.0.0.1')
  try {
    let text = ''
    const replies = new Promise<void>(resolve => socket.on('data', data => { text += data.toString(); if (text.split('200 OK').length - 1 === 3) resolve() }))
    socket.write(`GET /a.js HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\n\r\nPOST /api HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nContent-Length: 5\r\n\r\nhel`)
    socket.write(`loGET /b.js HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\n\r\n`)
    await replies
    expect(seen).toEqual(['GET /preview-proxy/T1/a.js HTTP/1.1|', 'POST /preview-proxy/T1/api HTTP/1.1|hello', 'GET /preview-proxy/T1/b.js HTTP/1.1|'])
    expect(connections).toBe(1)
  } finally { socket.destroy(); relay.close(); await new Promise<void>(resolve => upstream.close(() => resolve())) }
})
