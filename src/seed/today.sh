#!/usr/bin/env bash
# Heartbeat: emit today's date (day granularity) so the loop wakes once a day
# even when nothing else changes — lets daily/recurring goals fire on schedule.
echo "{\"date\":\"$(date +%F)\"}"
