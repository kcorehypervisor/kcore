import pytest

from kcore_agent_dashboard import service


@pytest.fixture(autouse=True)
def offline(monkeypatch):
    monkeypatch.setenv("KCORE_AGENT_OFFLINE", "1")
    monkeypatch.delenv("KCORE_AGENT_SNAPSHOT", raising=False)
    service.cache.clear()
