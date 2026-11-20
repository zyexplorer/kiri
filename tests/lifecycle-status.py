#!/usr/bin/env python3
"""Check idempotent start/stop and process exit before a zero-timeout stop returns."""
import http.client
import json
import socket
import sys
import time


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=10)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def check(path, image):
    owned = []

    def request(method, target, expected, body=None):
        connection = Connection(path)
        try:
            connection.request(method, "/v1.47" + target, None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            assert response.status in expected, (method, target, response.status, data)
            return response.status, json.loads(data) if data else None
        finally:
            connection.close()

    try:
        _, created = request("POST", "/containers/create", (201,),
                             {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": "none"}})
        cid = created["Id"]
        owned.append(cid)
        for _ in range(10):
            request("POST", "/containers/" + cid + "/start", (204,))
            request("POST", "/containers/" + cid + "/start", (304,))
            request("POST", "/containers/" + cid + "/stop?t=0", (204,))
            _, state = request("GET", "/containers/" + cid + "/json", (200,))
            assert not state["State"]["Running"], state["State"]
            request("POST", "/containers/" + cid + "/stop?t=0", (304,))
        print("PASS ten start/stop cycles and immediate exit inspection", flush=True)
        for _ in range(3):
            _, created = request("POST", "/containers/create", (201,),
                                 {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"AutoRemove": True}})
            cid = created["Id"]
            owned.append(cid)
            request("POST", "/containers/" + cid + "/start", (204,))
            request("POST", "/containers/" + cid + "/stop?t=0", (204,))
            status, state = request("GET", "/containers/" + cid + "/json", (200, 404))
            assert status == 404 or not state["State"]["Running"], state
        print("PASS three stops with automatic removal", flush=True)

        _, created = request("POST", "/containers/create", (201,),
                             {"Image": image, "Cmd": ["false"],
                              "HostConfig": {"NetworkMode": "none", "RestartPolicy": {"Name": "always"}}})
        cid = created["Id"]
        owned.append(cid)
        request("POST", "/containers/" + cid + "/start", (204,))
        # The delay before each restart doubles: after four of them the
        # process has exited and the next restart is most of a second away.
        deadline = time.monotonic() + 30
        while request("GET", "/containers/" + cid + "/json", (200,))[1]["RestartCount"] < 4:
            assert time.monotonic() < deadline, "the container did not keep restarting"
            time.sleep(0.05)
        request("POST", "/containers/" + cid + "/stop?t=0", (204,))
        _, stopped = request("GET", "/containers/" + cid + "/json", (200,))
        assert not stopped["State"]["Running"] and not stopped["State"]["Restarting"], stopped["State"]
        request("POST", "/containers/" + cid + "/stop?t=0", (304,))
        # Long enough for the restart that was pending to have happened.
        time.sleep(3)
        _, later = request("GET", "/containers/" + cid + "/json", (200,))
        assert not later["State"]["Running"] and not later["State"]["Restarting"], later["State"]
        assert later["RestartCount"] == stopped["RestartCount"], (stopped["RestartCount"], later["RestartCount"])
        print("PASS stop cancels a pending restart", flush=True)

        _, created = request("POST", "/containers/create", (201,),
                             {"Image": image, "Cmd": ["sleep", "120"], "HostConfig": {"NetworkMode": "none"}})
        cid = created["Id"]
        owned.append(cid)
        request("POST", "/containers/" + cid + "/start", (204,))
        request("POST", "/containers/" + cid + "/pause", (204,))
        _, error = request("POST", "/containers/" + cid + "/start", (409,))
        assert error["message"] == "cannot start a paused container, try unpause instead", error
        request("POST", "/containers/" + cid + "/unpause", (204,))
        request("POST", "/containers/" + cid + "/start", (304,))
        request("POST", "/containers/" + cid + "/pause", (204,))
        request("POST", "/containers/" + cid + "/stop?t=0", (204,))
        _, state = request("GET", "/containers/" + cid + "/json", (200,))
        assert not state["State"]["Running"] and not state["State"]["Paused"], state["State"]
        print("PASS start and stop of a paused container", flush=True)
    finally:
        primary_failure = sys.exc_info()[0] is not None
        errors = []
        for cid in reversed(owned):
            try:
                status, _ = request("DELETE", "/containers/" + cid + "?force=true", (204, 404, 409))
                if status == 409:
                    # AutoRemove can already be deleting this container.
                    # Wait on the engine's event instead of adding a sleep.
                    request("POST", "/containers/" + cid + "/wait?condition=removed", (200, 404))
                    request("GET", "/containers/" + cid + "/json", (404,))
            except Exception as error:
                errors.append(error)
                print("Cleanup failed:", repr(error), file=sys.stderr)
        if errors and not primary_failure:
            raise RuntimeError("Could not remove all lifecycle-test containers") from errors[0]


if __name__ == "__main__":
    check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine")
