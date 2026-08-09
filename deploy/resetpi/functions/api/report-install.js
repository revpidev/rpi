// Telemetry sink for rpi's install ping (telemetry.rs):
// `GET /api/report-install?version={version}` — fire-and-forget, the client
// ignores the response entirely. Returns 204 so Pages bills no egress body.
// Optionally record into a KV namespace ("ANALYTICS" binding in
// wrangler.toml) for install-count statistics: key = install:{version},
// value = ISO timestamp; delete or leave unbound to skip recording.
export async function onRequestGet({ request, env }) {
  const url = new URL(request.url);
  const version = url.searchParams.get("version") ?? "unknown";
  const ua = request.headers.get("user-agent") ?? "unknown";
  if (env.ANALYTICS) {
    await env.ANALYTICS.put(
      `install:${version}`,
      JSON.stringify({ at: new Date().toISOString(), ua }),
    );
  }
  return new Response(null, { status: 204 });
}
