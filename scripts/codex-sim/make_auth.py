#!/usr/bin/env python3
"""Fabricate a FAKE codex auth.json for the clauth switch simulation.

Tokens are unsigned garbage JWTs (matching clauth's testutil::fake_jwt shape) —
never real credentials. exp is set 10 days out and last_refresh to now so the
CDX-3 standby keep-alive's `standby_due` stays false (no network refresh ever
fires in the sandbox).
"""
import base64
import json
import sys
import time
from datetime import datetime, timezone


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def fake_jwt(claims: dict) -> str:
    header = b64url(json.dumps({"alg": "RS256", "typ": "JWT"}).encode())
    payload = b64url(json.dumps(claims).encode())
    return f"{header}.{payload}.fake-sig"


def auth_json(tag: str) -> str:
    now = int(time.time())
    account_id = f"acct-sim-{tag}"
    id_token = fake_jwt(
        {
            "email": f"sim-{tag}@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "pro",
                "chatgpt_account_id": account_id,
            },
        }
    )
    access_token = fake_jwt(
        {
            "exp": now + 10 * 24 * 3600,
            "sub": f"sim-{tag}",
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
        }
    )
    return json.dumps(
        {
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": f"rt-sim-{tag}-FAKE",
                "account_id": account_id,
            },
            "last_refresh": datetime.now(timezone.utc).isoformat(),
        },
        indent=2,
    )


if __name__ == "__main__":
    print(auth_json(sys.argv[1]))
