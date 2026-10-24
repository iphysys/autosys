import os
import sys
import time

import httpx as httpx
from openai import APIConnectionError, OpenAI


def _get_required_api_key() -> str:
    api_key = os.getenv("PLINY_API_KEY") or os.getenv("OPENAI_API_KEY")
    if not api_key:
        print("Missing API key.")
        print("Set PLINY_API_KEY (preferred) or OPENAI_API_KEY before running this script.")
        sys.exit(2)
    return api_key


def _build_http_client() -> httpx.Client:
    ca_bundle = (
        os.getenv("INTERNAL_CA_BUNDLE")
        or os.getenv("SSL_CERT_FILE")
        or os.getenv("REQUESTS_CA_BUNDLE")
    )
    # Allow opting out only for local troubleshooting.
    allow_insecure = os.getenv("ALLOW_INSECURE_TLS", "false").lower() in {
        "1",
        "true",
        "yes",
    }

    if allow_insecure:
        verify = False
    elif ca_bundle:
        verify = ca_bundle
    else:
        verify = True

    return httpx.Client(verify=verify, timeout=60.0)


client = OpenAI(
    api_key=_get_required_api_key(),
    base_url=os.getenv("PLINY_BASE_URL", "https://snps-inference.internal.synopsys.com/api/llm"),
    http_client=_build_http_client(),
)

print("Type your prompt and press Enter. Type 'exit' to quit.")

total_output_tokens = 0
total_generation_seconds = 0.0

while True:
    user_prompt = input("You: ").strip()
    if not user_prompt:
        continue
    if user_prompt.lower() in {"exit", "quit"}:
        print("Exiting.")
        break

    try:
        started_at = time.perf_counter()
        response = client.chat.completions.create(
            messages=[
                {"role": "system", "content": "You are an AI bot."},
                {"role": "user", "content": user_prompt},
            ],
            model="snps-provider/nemotron-3-ultra-550b-a55",
            # model="snps-provider/gpt-oss-120b-128k-9782f",
            stream=False,
            extra_headers={
                "X-TFY-METADATA": '{"your_custom_key":"your_custom_value"}',
                "X-TFY-LOGGING-CONFIG": '{"enabled":true}',
            },
        )
        elapsed = time.perf_counter() - started_at
        completion_tokens = getattr(response.usage, "completion_tokens", None)

        print("Assistant:", response.choices[0].message.content)
        print(f"Latency: {elapsed:.3f}s")

        if completion_tokens is not None and elapsed > 0:
            inst_tps = completion_tokens / elapsed
            total_output_tokens += completion_tokens
            total_generation_seconds += elapsed
            avg_tps = total_output_tokens / total_generation_seconds

            print(f"Output tokens: {completion_tokens}")
            print(f"Tokens/sec (this response): {inst_tps:.2f}")
            print(f"Avg tokens/sec (session): {avg_tps:.2f}")
        else:
            print("Token usage not available from provider for this response.")
    except APIConnectionError as exc:
        print("Connection failed. If this is an internal endpoint, configure your CA bundle.")
        print("Try setting INTERNAL_CA_BUNDLE=/path/to/internal-ca.pem")
        print("Temporary troubleshooting only: set ALLOW_INSECURE_TLS=true")
        print(f"Details: {exc}")
        sys.exit(2)
