#!/bin/sh
# Nightly sonicbrew regression wrapper: runs the suite, keeps 14 days of logs.
LOG=/root/sonicbrew/logs/regression-$(date +%Y%m%d-%H%M).log
sh /root/sonicbrew/scripts/freebsd-regression.sh > "$LOG" 2>&1
EXIT=$?
# Prune logs older than 14 days.
find /root/sonicbrew/logs -name "regression-*.log" -mtime +14 -delete 2>/dev/null
# Fail marker for quick checks.
if [ $EXIT -eq 0 ]; then rm -f /root/sonicbrew/logs/LAST_FAILED; else date > /root/sonicbrew/logs/LAST_FAILED; fi
exit $EXIT
