# Reference passes and look angles from Skyfield, for foxygnal's sat tests.
import sys
from skyfield.api import load, wgs84, EarthSatellite

ts = load.timescale(builtin=True)
lines = [l.rstrip() for l in open(sys.argv[1]) if l.strip()]
sats = {}
for i in range(0, len(lines), 3):
    s = EarthSatellite(lines[i + 1], lines[i + 2], lines[i].strip(), ts)
    sats[s.name] = s
obs = wgs84.latlon(52.52, 13.405, elevation_m=34.0)
names = ["METEOR-M2 3", "METEOR-M2 4", "METOP-B", "NOAA 20 (JPSS-1)"]
t0 = ts.utc(2026, 11, 23, 12, 0, 0)
t1 = ts.utc(2026, 11, 24, 12, 0, 0)
print("# Skyfield %s: observer 52.52 N 13.405 E 34 m, 2026-11-23T12:00Z + 24 h, horizon 0 deg" % sys.argv[2])
print("# event NAME KIND(0 rise,1 culminate,2 set) UNIX EL AZ")
print("# look NAME UNIX EL AZ RANGE_KM RANGE_RATE_KMS")
for n in names:
    s = sats[n]
    key = n.replace(" ", "_")
    t, ev = s.find_events(obs, t0, t1, altitude_degrees=0.0)
    for ti, e in zip(t, ev):
        alt, az, dist = (s - obs).at(ti).altaz()
        unix = ti.utc_datetime().timestamp()
        print("event", key, int(e), "%.3f" % unix, "%.4f" % alt.degrees, "%.4f" % az.degrees)
    for k in range(0, 24 * 60, 7):
        ti = ts.utc(2026, 11, 23, 12, k, 0)
        topo = (s - obs).at(ti)
        alt, az, dist, _, _, rr = topo.frame_latlon_and_rates(obs)
        unix = ti.utc_datetime().timestamp()
        print("look", key, "%.1f" % unix, "%.6f" % alt.degrees, "%.6f" % az.degrees,
              "%.6f" % dist.km, "%.9f" % rr.km_per_s)
