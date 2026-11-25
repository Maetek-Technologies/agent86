"""Write data/events.jsonl, the same every time: about 50,000 events from
October to December 2026, shuffled. Planted, beside random users who never
reach 25 purchases in a month:

- u0217 makes 34 purchases in November by UTC, six of them stamped with a
  +05:00 offset on 1 December local time (still 30 November in UTC);
- u0099 makes 31 in November plus five stamped 1 November local time at
  +05:00, which are 31 October in UTC, so a count by the local date gives 36;
- u0350 has 40 "purchase_refund" events in November and few purchases;
- u0123 makes 45 purchases, all in October and December.
"""

import datetime
import json
import pathlib
import random

UTC = datetime.timezone.utc
PLUS5 = datetime.timezone(datetime.timedelta(hours=5))
MINUS7 = datetime.timezone(datetime.timedelta(hours=-7))


def stamp(rng, month, tz=UTC):
    day = rng.randint(1, {10: 31, 11: 30, 12: 31}[month])
    moment = datetime.datetime(2026, month, day, rng.randint(0, 23), rng.randint(0, 59), rng.randint(0, 59), tzinfo=UTC)
    return moment.astimezone(tz).isoformat()


def main():
    rng = random.Random(86)
    events = []

    def add(user, kind, ts):
        events.append({"user": user, "type": kind, "ts": ts, "amount": round(rng.uniform(3, 250), 2)})

    users = [f"u{n:04d}" for n in range(1, 401) if n not in (99, 123, 217, 350)]
    for user in users:
        for month in (10, 11, 12):
            for _ in range(rng.randint(0, 24)):
                add(user, "purchase", stamp(rng, month, rng.choice([UTC, PLUS5, MINUS7])))
    while len(events) < 47_000:
        user = rng.choice(users)
        add(user, rng.choice(["view", "view", "view", "cart", "purchase_refund", "login"]),
            stamp(rng, rng.choice([10, 11, 12]), rng.choice([UTC, PLUS5, MINUS7])))

    for _ in range(28):
        add("u0217", "purchase", stamp(rng, 11))
    for hour in range(19, 25):
        moment = datetime.datetime(2026, 11, 30, hour - 1, 30, tzinfo=UTC)
        add("u0217", "purchase", moment.astimezone(PLUS5).isoformat())
    for _ in range(31):
        add("u0099", "purchase", stamp(rng, 11))
    for minute in range(5):
        moment = datetime.datetime(2026, 10, 31, 20, 10 * minute, tzinfo=UTC)
        add("u0099", "purchase", moment.astimezone(PLUS5).isoformat())
    for _ in range(40):
        add("u0350", "purchase_refund", stamp(rng, 11))
    for _ in range(3):
        add("u0350", "purchase", stamp(rng, 11))
    for _ in range(45):
        add("u0123", "purchase", stamp(rng, rng.choice([10, 12])))

    rng.shuffle(events)
    path = pathlib.Path("data")
    path.mkdir(exist_ok=True)
    with (path / "events.jsonl").open("w", encoding="utf-8") as handle:
        for number, event in enumerate(events, start=1):
            handle.write(json.dumps({"id": number, **event}) + "\n")


if __name__ == "__main__":
    main()
