import { createHash } from "node:crypto";
import { createServer, request as httpRequest } from "node:http";
import { cpus, platform, release } from "node:os";
import { mkdir, writeFile } from "node:fs/promises";
import { performance } from "node:perf_hooks";

const args = process.argv.slice(2);
const rawValueOf = (name) => {
  const index = args.indexOf(name);
  if (index < 0) return null;
  const value = args[index + 1];
  if (!value || value.startsWith("--")) throw new Error(`${name} 必須提供值`);
  return value;
};
const parseSafeInteger = (name, fallback, minimum, maximum) => {
  const raw = rawValueOf(name);
  if (raw === null) return fallback;
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new Error(`${name} 必須是 ${minimum}..${maximum} 的安全整數`);
  }
  return value;
};

const baselineArg = rawValueOf("--baseline-minutes");
const baselineMinutes = baselineArg === null ? null : Number(baselineArg);
if (baselineMinutes !== null && (!Number.isFinite(baselineMinutes) || baselineMinutes < 0)) {
  throw new Error("--baseline-minutes 必須是非負數");
}
const outDir = rawValueOf("--out-dir") ?? "benchmarks/out";
const fixtureMiB = parseSafeInteger("--fixture-mib", 16, 1, 512);
const repetitions = parseSafeInteger("--repetitions", 3, 3, 20);
const fixtureBytes = fixtureMiB * 1024 * 1024;
if (!Number.isSafeInteger(fixtureBytes) || fixtureBytes < 1024 * 1024) {
  throw new Error("--fixture-mib 必須是至少 1 MiB 的整數");
}

const fixture = Buffer.allocUnsafe(fixtureBytes);
for (let index = 0; index < fixture.length; index += 1) fixture[index] = (index * 31 + 17) & 0xff;
const fixtureChecksum = createHash("sha256").update(fixture).digest("hex");

function parseRange(value, size) {
  const match = /^bytes=(\d+)-(\d+)$/.exec(value ?? "");
  if (!match) return null;
  const start = Number(match[1]);
  const end = Number(match[2]);
  if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || end < start || end >= size) {
    return null;
  }
  return { start, end };
}

function createFixtureServer() {
  return createServer((request, response) => {
    if (request.method !== "GET" || request.url !== "/fixture") {
      response.writeHead(404).end();
      return;
    }
    const range = parseRange(request.headers.range, fixture.length);
    const start = range?.start ?? 0;
    const end = range?.end ?? fixture.length - 1;
    response.statusCode = range ? 206 : 200;
    response.setHeader("Accept-Ranges", "bytes");
    response.setHeader("Content-Type", "application/octet-stream");
    response.setHeader("Content-Length", end - start + 1);
    if (range) response.setHeader("Content-Range", `bytes ${start}-${end}/${fixture.length}`);
    response.end(fixture.subarray(start, end + 1));
  });
}

async function listen(server) {
  return new Promise((resolve, reject) => {
    const onError = (error) => reject(error);
    server.once("error", onError);
    server.listen({ host: "127.0.0.1", port: 0 }, () => {
      server.off("error", onError);
      const address = server.address();
      if (!address || typeof address === "string") {
        reject(new Error("loopback fixture server did not expose a TCP port"));
        return;
      }
      resolve(address.port);
    });
  });
}

async function closeServer(server) {
  if (!server.listening) return;
  await new Promise((resolve) => server.close(resolve));
}

function downloadRange(port, start, end, onChunk) {
  return new Promise((resolve, reject) => {
    const request = httpRequest(
      {
        host: "127.0.0.1",
        port,
        path: "/fixture",
        method: "GET",
        headers: { Range: `bytes=${start}-${end}` },
      },
      (response) => {
        let bytes = 0;
        response.on("data", (chunk) => {
          bytes += chunk.length;
          onChunk(chunk);
        });
        response.on("end", () => {
          const contentRange = response.headers["content-range"];
          const expectedRange = `bytes ${start}-${end}/${fixture.length}`;
          if (response.statusCode !== 206 || contentRange !== expectedRange) {
            reject(new Error(`fixture range response invalid: ${response.statusCode} ${contentRange ?? ""}`));
            return;
          }
          if (bytes !== end - start + 1) {
            reject(new Error(`fixture range length invalid: ${bytes}`));
            return;
          }
          resolve(bytes);
        });
        response.on("error", reject);
      },
    );
    request.on("error", reject);
    request.end();
  });
}

async function processFixture(port, fragmentCount) {
  const hash = createHash("sha256");
  const fragmentSize = Math.ceil(fixture.length / fragmentCount);
  let downloaded = 0;
  for (let offset = 0; offset < fixture.length; offset += fragmentSize) {
    const end = Math.min(offset + fragmentSize, fixture.length) - 1;
    downloaded += await downloadRange(port, offset, end, (chunk) => hash.update(chunk));
  }
  const checksum = hash.digest("hex");
  if (downloaded !== fixture.length || checksum !== fixtureChecksum) {
    throw new Error("fixture download integrity check failed");
  }
  return checksum;
}

const server = createFixtureServer();
const port = await listen(server);
const measurements = [];
try {
  for (const concurrency of [1, 2, 3, 5]) {
    for (const fragments of [1, 2, 4, 8]) {
      const elapsedRuns = [];
      let checksums = [];
      for (let repeat = 0; repeat < repetitions; repeat += 1) {
        const started = performance.now();
        checksums = await Promise.all(
          Array.from({ length: concurrency }, () => processFixture(port, fragments)),
        );
        elapsedRuns.push(performance.now() - started);
      }
      if (checksums.some((checksum) => checksum !== fixtureChecksum)) {
        throw new Error("fixture checksum mismatch");
      }
      elapsedRuns.sort((left, right) => left - right);
      const elapsedMs = elapsedRuns[Math.floor(elapsedRuns.length / 2)];
      measurements.push({
        concurrency,
        fragments,
        repetitions,
        elapsedMs: Number(elapsedMs.toFixed(2)),
        throughputMiBPerSecond: Number(((fixtureBytes * concurrency / 1024 / 1024) / (elapsedMs / 1000)).toFixed(2)),
        checksum: fixtureChecksum,
      });
    }
  }
} finally {
  await closeServer(server);
}

const gate = baselineMinutes === null ? "not-evaluated" : baselineMinutes > 20 ? "fail" : "pass";
const result = {
  schemaVersion: 2,
  fixtureOnly: true,
  transport: "loopback-http-range",
  ssrfAllowlistApplied: false,
  generatedAt: new Date().toISOString(),
  baselineDurationMinutes: baselineMinutes,
  gate,
  fixture: {
    sizeMiB: fixtureMiB,
    repetitions,
    fragmentMeaning: "loopback HTTP Range 分塊數；不是 yt-dlp 網路 fragment 數",
  },
  environment: { platform: platform(), release: release(), cpuCount: cpus().length },
  measurements,
  bottleneckAdvice: gate === "fail"
    ? "明確提供的固定基準影片超過 20 分鐘：檢查磁碟 I/O、ffmpeg postprocess、fragment 數與網路重試；不得以 fixture 結果宣稱 live 站台效能。"
    : gate === "not-evaluated"
      ? "尚未提供外部 live baseline，因此 gate 不評估；loopback fixture 數據不能冒充 yt-dlp、ffmpeg 或 live 站台結果。"
      : "目前僅為離線 loopback fixture gate；要評估 live 站台仍須使用公開且明確有權下載的內容。",
};
await mkdir(outDir, { recursive: true });
await writeFile(`${outDir}/benchmark-results.json`, `${JSON.stringify(result, null, 2)}\n`, "utf8");
const lines = [
  "# Fixture Benchmark",
  "",
  `- Gate: **${gate}**`,
  `- Fixture only: **${result.fixtureOnly}**`,
  `- Transport: **loopback HTTP Range** (application SSRF allowlist not applied)`,
  `- Baseline duration: ${baselineMinutes === null ? "not supplied" : `${baselineMinutes} minutes`}`,
  `- Fixture size/repetitions: ${fixtureMiB} MiB / ${repetitions} (median)`,
  "",
  "| concurrency | fragments | elapsed (ms) | throughput (MiB/s) |",
  "| ---: | ---: | ---: | ---: |",
  ...measurements.map((item) => `| ${item.concurrency} | ${item.fragments} | ${item.elapsedMs} | ${item.throughputMiBPerSecond} |`),
  "",
  `> ${result.bottleneckAdvice}`,
];
await writeFile(`${outDir}/benchmark-results.md`, `${lines.join("\n")}\n`, "utf8");
console.log(JSON.stringify({ outDir, gate, measurements: measurements.length }, null, 2));
