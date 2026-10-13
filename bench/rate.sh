#!/usr/bin/env bash
# rate.sh <secs> : server-side capture/encode rates over a window from /stats
a=$(curl -s localhost:6902/stats); sleep $1; b=$(curl -s localhost:6902/stats)
python3 -c "
import json,sys
a=json.loads('''$a'''); b=json.loads('''$b'''); d={k:b[k]-a[k] for k in a if isinstance(a[k],int)}; s=$1
n=max(d['encoded'],1)
print(f\"captured {d['captured']/s:.1f}/s encoded {d['encoded']/s:.1f}/s  {d['bytes']*8/s/1e6:.2f} Mbps  grab {d['grab_us_total']/n/1000:.2f}ms conv {d['convert_us_total']/n/1000:.2f}ms enc {d['encode_us_total']/n/1000:.2f}ms  dmgwait {d['damage_wait_us_total']/n/1000:.2f}ms dmg->sent {d['send_lat_us_total']/n/1000:.2f}ms flow_blocked {d['flow_blocked']} paintovers {d['paintovers']} key {d['keyframes']}\")
print(b['clients'])
"
