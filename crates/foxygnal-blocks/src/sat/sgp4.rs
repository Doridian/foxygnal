//! SGP4 / SDP4: the NORAD simplified perturbation models that two-line
//! element sets are made for.
//!
//! A straight port of David Vallado's reference implementation
//! (`sgp4unit.cpp`, the 2020 version accompanying "Revisiting Spacetrack
//! Report #3", AIAA 2006-6753) with WGS-72 constants and the "improved"
//! operation mode (`opsmode = 'i'`), which is what the published
//! verification output (`tcppver.out`) was made with. The deep-space
//! resonance integrator always restarts from epoch, as Vallado's code does
//! since 2018, so a propagation doesn't depend on earlier calls and
//! [`Sgp4::propagate`] takes `&self`.
//!
//! Names follow Vallado's so the two can be read side by side.

use std::f64::consts::{PI, TAU};

use super::tle::Tle;

/// WGS-72 constants, as SGP4 uses them (not WGS-84: the elements are fitted
/// with these).
const MU: f64 = 398_600.8; // km³/s²
pub const RADIUS_EARTH_KM: f64 = 6378.135;
const J2: f64 = 0.001_082_616;
const J3: f64 = -0.000_002_538_81;
const J4: f64 = -0.000_001_655_97;
const J3OJ2: f64 = J3 / J2;
const X2O3: f64 = 2.0 / 3.0;

fn xke() -> f64 {
    60.0 / (RADIUS_EARTH_KM * RADIUS_EARTH_KM * RADIUS_EARTH_KM / MU).sqrt()
}

/// Why a propagation failed (Vallado's error codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sgp4Error {
    /// 1: mean eccentricity out of range.
    Eccentricity,
    /// 2: mean motion below zero.
    MeanMotion,
    /// 3: perturbed eccentricity out of range.
    PerturbedEccentricity,
    /// 4: semi-latus rectum below zero.
    SemiLatusRectum,
    /// 6: the orbit has decayed (radius below one Earth radius).
    Decayed,
}

impl std::fmt::Display for Sgp4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Eccentricity => "mean eccentricity out of range",
            Self::MeanMotion => "mean motion below zero",
            Self::PerturbedEccentricity => "perturbed eccentricity out of range",
            Self::SemiLatusRectum => "semi-latus rectum below zero",
            Self::Decayed => "the satellite has decayed",
        })
    }
}

/// Position (km) and velocity (km/s) in the TEME frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct State {
    pub r: [f64; 3],
    pub v: [f64; 3],
}

/// Greenwich mean sidereal time (rad) at a UT1 Julian date (IAU 1982).
pub fn gstime(jdut1: f64) -> f64 {
    let tut1 = (jdut1 - 2_451_545.0) / 36_525.0;
    let temp = -6.2e-6 * tut1 * tut1 * tut1
        + 0.093_104 * tut1 * tut1
        + (876_600.0 * 3600.0 + 8_640_184.812_866) * tut1
        + 67_310.548_41;
    let temp = (temp * (PI / 180.0) / 240.0) % TAU;
    if temp < 0.0 { temp + TAU } else { temp }
}

/// Deep-space (SDP4) constants, set up once.
#[derive(Debug, Clone, Default)]
struct Deep {
    irez: u8,
    d2201: f64,
    d2211: f64,
    d3210: f64,
    d3222: f64,
    d4410: f64,
    d4422: f64,
    d5220: f64,
    d5232: f64,
    d5421: f64,
    d5433: f64,
    dedt: f64,
    del1: f64,
    del2: f64,
    del3: f64,
    didt: f64,
    dmdt: f64,
    dnodt: f64,
    domdt: f64,
    e3: f64,
    ee2: f64,
    peo: f64,
    pgho: f64,
    pho: f64,
    pinco: f64,
    plo: f64,
    se2: f64,
    se3: f64,
    sgh2: f64,
    sgh3: f64,
    sgh4: f64,
    sh2: f64,
    sh3: f64,
    si2: f64,
    si3: f64,
    sl2: f64,
    sl3: f64,
    sl4: f64,
    xfact: f64,
    xgh2: f64,
    xgh3: f64,
    xgh4: f64,
    xh2: f64,
    xh3: f64,
    xi2: f64,
    xi3: f64,
    xl2: f64,
    xl3: f64,
    xl4: f64,
    xlamo: f64,
    zmol: f64,
    zmos: f64,
}

/// An initialised SGP4 model for one element set.
#[derive(Debug, Clone)]
pub struct Sgp4 {
    /// The elements' epoch as a Unix time (s).
    pub epoch_unix: f64,
    deep: Option<Box<Deep>>,
    isimp: bool,
    // Elements.
    bstar: f64,
    ecco: f64,
    argpo: f64,
    inclo: f64,
    mo: f64,
    nodeo: f64,
    no_unkozai: f64,
    // Near-earth constants.
    aycof: f64,
    con41: f64,
    cc1: f64,
    cc4: f64,
    cc5: f64,
    d2: f64,
    d3: f64,
    d4: f64,
    delmo: f64,
    eta: f64,
    argpdot: f64,
    omgcof: f64,
    sinmao: f64,
    t2cof: f64,
    t3cof: f64,
    t4cof: f64,
    t5cof: f64,
    x1mth2: f64,
    x7thm1: f64,
    mdot: f64,
    nodedot: f64,
    xlcof: f64,
    xmcof: f64,
    nodecf: f64,
    gsto: f64,
}

/// What `dscom` computes, kept for `dsinit`.
#[derive(Default)]
struct Dscom {
    sinim: f64,
    cosim: f64,
    emsq: f64,
    s1: f64,
    s2: f64,
    s3: f64,
    s4: f64,
    s5: f64,
    ss1: f64,
    ss2: f64,
    ss3: f64,
    ss4: f64,
    ss5: f64,
    sz1: f64,
    sz3: f64,
    sz11: f64,
    sz13: f64,
    sz21: f64,
    sz23: f64,
    sz31: f64,
    sz33: f64,
    z1: f64,
    z3: f64,
    z11: f64,
    z13: f64,
    z21: f64,
    z23: f64,
    z31: f64,
    z33: f64,
}

impl Sgp4 {
    /// Set up the model for `tle` (Vallado's `sgp4init`).
    pub fn new(tle: &Tle) -> Result<Self, Sgp4Error> {
        let xke = xke();
        let re = RADIUS_EARTH_KM;
        let xpdotp = 1440.0 / TAU;
        let deg = PI / 180.0;
        let no_kozai = tle.mean_motion / xpdotp;
        let ecco = tle.eccentricity;
        let inclo = tle.inclination * deg;
        let nodeo = tle.raan * deg;
        let argpo = tle.arg_perigee * deg;
        let mo = tle.mean_anomaly * deg;
        let bstar = tle.bstar;
        let epoch = tle.epoch_1950();
        let epoch_unix = tle.epoch_unix();

        let ss = 78.0 / re + 1.0;
        let qzms2t = ((120.0 - 78.0) / re).powi(4);

        // initl
        let eccsq = ecco * ecco;
        let omeosq = 1.0 - eccsq;
        let rteosq = omeosq.sqrt();
        let cosio = inclo.cos();
        let cosio2 = cosio * cosio;
        let ak = (xke / no_kozai).powf(X2O3);
        let d1 = 0.75 * J2 * (3.0 * cosio2 - 1.0) / (rteosq * omeosq);
        let mut del = d1 / (ak * ak);
        let adel = ak * (1.0 - del * del - del * (1.0 / 3.0 + 134.0 * del * del / 81.0));
        del = d1 / (adel * adel);
        let no_unkozai = no_kozai / (1.0 + del);
        let ao = (xke / no_unkozai).powf(X2O3);
        let sinio = inclo.sin();
        let po = ao * omeosq;
        let con42 = 1.0 - 5.0 * cosio2;
        let con41 = -con42 - cosio2 - cosio2;
        let posq = po * po;
        let rp = ao * (1.0 - ecco);
        let gsto = gstime(epoch + 2_433_281.5);

        let mut s = Sgp4 {
            epoch_unix,
            deep: None,
            isimp: false,
            bstar,
            ecco,
            argpo,
            inclo,
            mo,
            nodeo,
            no_unkozai,
            aycof: 0.0,
            con41,
            cc1: 0.0,
            cc4: 0.0,
            cc5: 0.0,
            d2: 0.0,
            d3: 0.0,
            d4: 0.0,
            delmo: 0.0,
            eta: 0.0,
            argpdot: 0.0,
            omgcof: 0.0,
            sinmao: 0.0,
            t2cof: 0.0,
            t3cof: 0.0,
            t4cof: 0.0,
            t5cof: 0.0,
            x1mth2: 0.0,
            x7thm1: 0.0,
            mdot: 0.0,
            nodedot: 0.0,
            xlcof: 0.0,
            xmcof: 0.0,
            nodecf: 0.0,
            gsto,
        };

        if omeosq >= 0.0 || no_unkozai >= 0.0 {
            s.isimp = rp < 220.0 / re + 1.0;
            let mut sfour = ss;
            let mut qzms24 = qzms2t;
            let perige = (rp - 1.0) * re;
            if perige < 156.0 {
                sfour = perige - 78.0;
                if perige < 98.0 {
                    sfour = 20.0;
                }
                qzms24 = ((120.0 - sfour) / re).powi(4);
                sfour = sfour / re + 1.0;
            }
            let pinvsq = 1.0 / posq;
            let tsi = 1.0 / (ao - sfour);
            s.eta = ao * ecco * tsi;
            let etasq = s.eta * s.eta;
            let eeta = ecco * s.eta;
            let psisq = (1.0 - etasq).abs();
            let coef = qzms24 * tsi.powi(4);
            let coef1 = coef / psisq.powf(3.5);
            let cc2 = coef1
                * no_unkozai
                * (ao * (1.0 + 1.5 * etasq + eeta * (4.0 + etasq))
                    + 0.375 * J2 * tsi / psisq * con41 * (8.0 + 3.0 * etasq * (8.0 + etasq)));
            s.cc1 = bstar * cc2;
            let mut cc3 = 0.0;
            if ecco > 1.0e-4 {
                cc3 = -2.0 * coef * tsi * J3OJ2 * no_unkozai * sinio / ecco;
            }
            s.x1mth2 = 1.0 - cosio2;
            s.cc4 = 2.0
                * no_unkozai
                * coef1
                * ao
                * omeosq
                * (s.eta * (2.0 + 0.5 * etasq) + ecco * (0.5 + 2.0 * etasq)
                    - J2 * tsi / (ao * psisq)
                        * (-3.0 * con41 * (1.0 - 2.0 * eeta + etasq * (1.5 - 0.5 * eeta))
                            + 0.75
                                * s.x1mth2
                                * (2.0 * etasq - eeta * (1.0 + etasq))
                                * (2.0 * argpo).cos()));
            s.cc5 = 2.0 * coef1 * ao * omeosq * (1.0 + 2.75 * (etasq + eeta) + eeta * etasq);
            let cosio4 = cosio2 * cosio2;
            let temp1 = 1.5 * J2 * pinvsq * no_unkozai;
            let temp2 = 0.5 * temp1 * J2 * pinvsq;
            let temp3 = -0.46875 * J4 * pinvsq * pinvsq * no_unkozai;
            s.mdot = no_unkozai
                + 0.5 * temp1 * rteosq * con41
                + 0.0625 * temp2 * rteosq * (13.0 - 78.0 * cosio2 + 137.0 * cosio4);
            s.argpdot = -0.5 * temp1 * con42
                + 0.0625 * temp2 * (7.0 - 114.0 * cosio2 + 395.0 * cosio4)
                + temp3 * (3.0 - 36.0 * cosio2 + 49.0 * cosio4);
            let xhdot1 = -temp1 * cosio;
            s.nodedot = xhdot1
                + (0.5 * temp2 * (4.0 - 19.0 * cosio2) + 2.0 * temp3 * (3.0 - 7.0 * cosio2))
                    * cosio;
            let xpidot = s.argpdot + s.nodedot;
            s.omgcof = bstar * cc3 * argpo.cos();
            if ecco > 1.0e-4 {
                s.xmcof = -X2O3 * coef * bstar / eeta;
            }
            s.nodecf = 3.5 * omeosq * xhdot1 * s.cc1;
            s.t2cof = 1.5 * s.cc1;
            s.xlcof = if (cosio + 1.0).abs() > 1.5e-12 {
                -0.25 * J3OJ2 * sinio * (3.0 + 5.0 * cosio) / (1.0 + cosio)
            } else {
                -0.25 * J3OJ2 * sinio * (3.0 + 5.0 * cosio) / 1.5e-12
            };
            s.aycof = -0.5 * J3OJ2 * sinio;
            s.delmo = (1.0 + s.eta * mo.cos()).powi(3);
            s.sinmao = mo.sin();
            s.x7thm1 = 7.0 * cosio2 - 1.0;

            if TAU / no_unkozai >= 225.0 {
                s.isimp = true;
                let mut d = Deep::default();
                let c = dscom(epoch, ecco, argpo, 0.0, inclo, nodeo, no_unkozai, &mut d);
                dsinit(
                    &mut d, &c, xke, argpo, gsto, mo, s.mdot, no_unkozai, nodeo, s.nodedot, xpidot,
                    ecco, eccsq, inclo,
                );
                s.deep = Some(Box::new(d));
            }

            if !s.isimp {
                let cc1sq = s.cc1 * s.cc1;
                s.d2 = 4.0 * ao * tsi * cc1sq;
                let temp = s.d2 * tsi * s.cc1 / 3.0;
                s.d3 = (17.0 * ao + sfour) * temp;
                s.d4 = 0.5 * temp * ao * tsi * (221.0 * ao + 31.0 * sfour) * s.cc1;
                s.t3cof = s.d2 + 2.0 * cc1sq;
                s.t4cof = 0.25 * (3.0 * s.d3 + s.cc1 * (12.0 * s.d2 + 10.0 * cc1sq));
                s.t5cof = 0.2
                    * (3.0 * s.d4
                        + 12.0 * s.cc1 * s.d3
                        + 6.0 * s.d2 * s.d2
                        + 15.0 * cc1sq * (2.0 * s.d2 + cc1sq));
            }
        }
        // Vallado's sgp4init ends with a propagation to epoch, whose errors
        // it reports.
        s.propagate(0.0)?;
        Ok(s)
    }

    /// True if the deep-space (SDP4) terms are used (period ≥ 225 min).
    pub fn is_deep_space(&self) -> bool {
        self.deep.is_some()
    }

    /// Position and velocity (TEME) at a Unix time (s).
    pub fn propagate_unix(&self, unix: f64) -> Result<State, Sgp4Error> {
        self.propagate((unix - self.epoch_unix) / 60.0)
    }

    /// Position and velocity (TEME) `tsince` minutes after the epoch.
    pub fn propagate(&self, tsince: f64) -> Result<State, Sgp4Error> {
        let xke = xke();
        let re = RADIUS_EARTH_KM;
        let vkmpersec = re * xke / 60.0;
        let t = tsince;

        let xmdf = self.mo + self.mdot * t;
        let argpdf = self.argpo + self.argpdot * t;
        let nodedf = self.nodeo + self.nodedot * t;
        let mut argpm = argpdf;
        let mut mm = xmdf;
        let t2 = t * t;
        let mut nodem = nodedf + self.nodecf * t2;
        let mut tempa = 1.0 - self.cc1 * t;
        let mut tempe = self.bstar * self.cc4 * t;
        let mut templ = self.t2cof * t2;

        if !self.isimp {
            let delomg = self.omgcof * t;
            let delmtemp = 1.0 + self.eta * xmdf.cos();
            let delm = self.xmcof * (delmtemp * delmtemp * delmtemp - self.delmo);
            let temp = delomg + delm;
            mm = xmdf + temp;
            argpm = argpdf - temp;
            let t3 = t2 * t;
            let t4 = t3 * t;
            tempa = tempa - self.d2 * t2 - self.d3 * t3 - self.d4 * t4;
            tempe += self.bstar * self.cc5 * (mm.sin() - self.sinmao);
            templ += self.t3cof * t3 + t4 * (self.t4cof + t * self.t5cof);
        }

        let mut nm = self.no_unkozai;
        let mut em = self.ecco;
        let mut inclm = self.inclo;
        if let Some(d) = &self.deep {
            dspace(
                d,
                self.argpo,
                self.argpdot,
                t,
                self.gsto,
                self.no_unkozai,
                &mut em,
                &mut argpm,
                &mut inclm,
                &mut mm,
                &mut nodem,
                &mut nm,
            );
        }

        if nm <= 0.0 {
            return Err(Sgp4Error::MeanMotion);
        }
        let am = (xke / nm).powf(X2O3) * tempa * tempa;
        nm = xke / am.powf(1.5);
        em -= tempe;
        if !(-0.001..1.0).contains(&em) {
            return Err(Sgp4Error::Eccentricity);
        }
        if em < 1.0e-6 {
            em = 1.0e-6;
        }
        mm += self.no_unkozai * templ;
        let mut xlm = mm + argpm + nodem;
        nodem %= TAU;
        argpm %= TAU;
        xlm %= TAU;
        mm = (xlm - argpm - nodem) % TAU;

        let sinim = inclm.sin();
        let cosim = inclm.cos();

        let mut ep = em;
        let mut xincp = inclm;
        let mut argpp = argpm;
        let mut nodep = nodem;
        let mut mp = mm;
        let mut sinip = sinim;
        let mut cosip = cosim;
        let mut aycof = self.aycof;
        let mut xlcof = self.xlcof;
        let (mut con41, mut x1mth2, mut x7thm1) = (self.con41, self.x1mth2, self.x7thm1);
        if let Some(d) = &self.deep {
            dpper(
                d, t, false, &mut ep, &mut xincp, &mut nodep, &mut argpp, &mut mp,
            );
            if xincp < 0.0 {
                xincp = -xincp;
                nodep += PI;
                argpp -= PI;
            }
            if !(0.0..=1.0).contains(&ep) {
                return Err(Sgp4Error::PerturbedEccentricity);
            }
            sinip = xincp.sin();
            cosip = xincp.cos();
            aycof = -0.5 * J3OJ2 * sinip;
            xlcof = if (cosip + 1.0).abs() > 1.5e-12 {
                -0.25 * J3OJ2 * sinip * (3.0 + 5.0 * cosip) / (1.0 + cosip)
            } else {
                -0.25 * J3OJ2 * sinip * (3.0 + 5.0 * cosip) / 1.5e-12
            };
        }

        // Long-period periodics.
        let axnl = ep * argpp.cos();
        let mut temp = 1.0 / (am * (1.0 - ep * ep));
        let aynl = ep * argpp.sin() + temp * aycof;
        let xl = mp + argpp + nodep + temp * xlcof * axnl;

        // Kepler's equation.
        let u = (xl - nodep) % TAU;
        let mut eo1 = u;
        let mut tem5: f64 = 9999.9;
        let mut ktr = 1;
        let (mut sineo1, mut coseo1) = (0.0, 0.0);
        while tem5.abs() >= 1.0e-12 && ktr <= 10 {
            sineo1 = eo1.sin();
            coseo1 = eo1.cos();
            tem5 = 1.0 - coseo1 * axnl - sineo1 * aynl;
            tem5 = (u - aynl * coseo1 + axnl * sineo1 - eo1) / tem5;
            if tem5.abs() >= 0.95 {
                tem5 = if tem5 > 0.0 { 0.95 } else { -0.95 };
            }
            eo1 += tem5;
            ktr += 1;
        }

        // Short-period periodics.
        let ecose = axnl * coseo1 + aynl * sineo1;
        let esine = axnl * sineo1 - aynl * coseo1;
        let el2 = axnl * axnl + aynl * aynl;
        let pl = am * (1.0 - el2);
        if pl < 0.0 {
            return Err(Sgp4Error::SemiLatusRectum);
        }
        let rl = am * (1.0 - ecose);
        let rdotl = am.sqrt() * esine / rl;
        let rvdotl = pl.sqrt() / rl;
        let betal = (1.0 - el2).sqrt();
        temp = esine / (1.0 + betal);
        let sinu = am / rl * (sineo1 - aynl - axnl * temp);
        let cosu = am / rl * (coseo1 - axnl + aynl * temp);
        let mut su = sinu.atan2(cosu);
        let sin2u = (cosu + cosu) * sinu;
        let cos2u = 1.0 - 2.0 * sinu * sinu;
        temp = 1.0 / pl;
        let temp1 = 0.5 * J2 * temp;
        let temp2 = temp1 * temp;

        if self.deep.is_some() {
            let cosisq = cosip * cosip;
            con41 = 3.0 * cosisq - 1.0;
            x1mth2 = 1.0 - cosisq;
            x7thm1 = 7.0 * cosisq - 1.0;
        }
        let mrt = rl * (1.0 - 1.5 * temp2 * betal * con41) + 0.5 * temp1 * x1mth2 * cos2u;
        su -= 0.25 * temp2 * x7thm1 * sin2u;
        let xnode = nodep + 1.5 * temp2 * cosip * sin2u;
        let xinc = xincp + 1.5 * temp2 * cosip * sinip * cos2u;
        let mvt = rdotl - nm * temp1 * x1mth2 * sin2u / xke;
        let rvdot = rvdotl + nm * temp1 * (x1mth2 * cos2u + 1.5 * con41) / xke;

        // Orientation vectors.
        let (sinsu, cossu) = su.sin_cos();
        let (snod, cnod) = xnode.sin_cos();
        let (sini, cosi) = xinc.sin_cos();
        let xmx = -snod * cosi;
        let xmy = cnod * cosi;
        let ux = xmx * sinsu + cnod * cossu;
        let uy = xmy * sinsu + snod * cossu;
        let uz = sini * sinsu;
        let vx = xmx * cossu - cnod * sinsu;
        let vy = xmy * cossu - snod * sinsu;
        let vz = sini * cossu;

        let state = State {
            r: [mrt * ux * re, mrt * uy * re, mrt * uz * re],
            v: [
                (mvt * ux + rvdot * vx) * vkmpersec,
                (mvt * uy + rvdot * vy) * vkmpersec,
                (mvt * uz + rvdot * vz) * vkmpersec,
            ],
        };
        if mrt < 1.0 {
            return Err(Sgp4Error::Decayed);
        }
        Ok(state)
    }
}

/// Lunar and solar terms that depend only on the epoch (Vallado's `dscom`).
#[allow(clippy::too_many_arguments)]
fn dscom(
    epoch: f64,
    ep: f64,
    argpp: f64,
    tc: f64,
    inclp: f64,
    nodep: f64,
    np: f64,
    d: &mut Deep,
) -> Dscom {
    const ZES: f64 = 0.01675;
    const ZEL: f64 = 0.05490;
    const C1SS: f64 = 2.986_479_7e-6;
    const C1L: f64 = 4.796_806_5e-7;
    const ZSINIS: f64 = 0.397_854_16;
    const ZCOSIS: f64 = 0.917_448_67;
    const ZCOSGS: f64 = 0.194_590_5;
    const ZSINGS: f64 = -0.980_884_58;

    let nm = np;
    let em = ep;
    let snodm = nodep.sin();
    let cnodm = nodep.cos();
    let sinomm = argpp.sin();
    let cosomm = argpp.cos();
    let sinim = inclp.sin();
    let cosim = inclp.cos();
    let emsq = em * em;
    let betasq = 1.0 - emsq;
    let rtemsq = betasq.sqrt();

    d.peo = 0.0;
    d.pinco = 0.0;
    d.plo = 0.0;
    d.pgho = 0.0;
    d.pho = 0.0;
    let day = epoch + 18_261.5 + tc / 1440.0;
    let xnodce = (4.523_602_0 - 9.242_202_9e-4 * day) % TAU;
    let stem = xnodce.sin();
    let ctem = xnodce.cos();
    let zcosil = 0.913_751_64 - 0.035_680_96 * ctem;
    let zsinil = (1.0 - zcosil * zcosil).sqrt();
    let zsinhl = 0.089_683_511 * stem / zsinil;
    let zcoshl = (1.0 - zsinhl * zsinhl).sqrt();
    let gam = 5.835_151_4 + 0.001_944_368_0 * day;
    let mut zx = 0.397_854_16 * stem / zsinil;
    let zy = zcoshl * ctem + 0.917_448_67 * zsinhl * stem;
    zx = zx.atan2(zy);
    zx = gam + zx - xnodce;
    let zcosgl = zx.cos();
    let zsingl = zx.sin();

    let mut zcosg = ZCOSGS;
    let mut zsing = ZSINGS;
    let mut zcosi = ZCOSIS;
    let mut zsini = ZSINIS;
    let mut zcosh = cnodm;
    let mut zsinh = snodm;
    let mut cc = C1SS;
    let xnoi = 1.0 / nm;

    let mut c = Dscom {
        sinim,
        cosim,
        emsq,
        ..Dscom::default()
    };
    // Solar first (lsflg 1), then lunar (lsflg 2).
    for lsflg in 1..=2 {
        let a1 = zcosg * zcosh + zsing * zcosi * zsinh;
        let a3 = -zsing * zcosh + zcosg * zcosi * zsinh;
        let a7 = -zcosg * zsinh + zsing * zcosi * zcosh;
        let a8 = zsing * zsini;
        let a9 = zsing * zsinh + zcosg * zcosi * zcosh;
        let a10 = zcosg * zsini;
        let a2 = cosim * a7 + sinim * a8;
        let a4 = cosim * a9 + sinim * a10;
        let a5 = -sinim * a7 + cosim * a8;
        let a6 = -sinim * a9 + cosim * a10;

        let x1 = a1 * cosomm + a2 * sinomm;
        let x2 = a3 * cosomm + a4 * sinomm;
        let x3 = -a1 * sinomm + a2 * cosomm;
        let x4 = -a3 * sinomm + a4 * cosomm;
        let x5 = a5 * sinomm;
        let x6 = a6 * sinomm;
        let x7 = a5 * cosomm;
        let x8 = a6 * cosomm;

        let z31 = 12.0 * x1 * x1 - 3.0 * x3 * x3;
        let z32 = 24.0 * x1 * x2 - 6.0 * x3 * x4;
        let z33 = 12.0 * x2 * x2 - 3.0 * x4 * x4;
        let mut z1 = 3.0 * (a1 * a1 + a2 * a2) + z31 * emsq;
        let mut z2 = 6.0 * (a1 * a3 + a2 * a4) + z32 * emsq;
        let mut z3 = 3.0 * (a3 * a3 + a4 * a4) + z33 * emsq;
        let z11 = -6.0 * a1 * a5 + emsq * (-24.0 * x1 * x7 - 6.0 * x3 * x5);
        let z12 = -6.0 * (a1 * a6 + a3 * a5)
            + emsq * (-24.0 * (x2 * x7 + x1 * x8) - 6.0 * (x3 * x6 + x4 * x5));
        let z13 = -6.0 * a3 * a6 + emsq * (-24.0 * x2 * x8 - 6.0 * x4 * x6);
        let z21 = 6.0 * a2 * a5 + emsq * (24.0 * x1 * x5 - 6.0 * x3 * x7);
        let z22 = 6.0 * (a4 * a5 + a2 * a6)
            + emsq * (24.0 * (x2 * x5 + x1 * x6) - 6.0 * (x4 * x7 + x3 * x8));
        let z23 = 6.0 * a4 * a6 + emsq * (24.0 * x2 * x6 - 6.0 * x4 * x8);
        z1 = z1 + z1 + betasq * z31;
        z2 = z2 + z2 + betasq * z32;
        z3 = z3 + z3 + betasq * z33;
        let s3 = cc * xnoi;
        let s2 = -0.5 * s3 / rtemsq;
        let s4 = s3 * rtemsq;
        let s1 = -15.0 * em * s4;
        let s5 = x1 * x3 + x2 * x4;
        let s6 = x2 * x3 + x1 * x4;
        let s7 = x2 * x4 - x1 * x3;

        if lsflg == 1 {
            c.ss1 = s1;
            c.ss2 = s2;
            c.ss3 = s3;
            c.ss4 = s4;
            c.ss5 = s5;
            c.sz1 = z1;
            c.sz3 = z3;
            c.sz11 = z11;
            c.sz13 = z13;
            c.sz21 = z21;
            c.sz23 = z23;
            c.sz31 = z31;
            c.sz33 = z33;
            // Solar terms of dpper.
            d.se2 = 2.0 * s1 * s6;
            d.se3 = 2.0 * s1 * s7;
            d.si2 = 2.0 * s2 * z12;
            d.si3 = 2.0 * s2 * (z13 - z11);
            d.sl2 = -2.0 * s3 * z2;
            d.sl3 = -2.0 * s3 * (z3 - z1);
            d.sl4 = -2.0 * s3 * (-21.0 - 9.0 * emsq) * ZES;
            d.sgh2 = 2.0 * s4 * z32;
            d.sgh3 = 2.0 * s4 * (z33 - z31);
            d.sgh4 = -18.0 * s4 * ZES;
            d.sh2 = -2.0 * s2 * z22;
            d.sh3 = -2.0 * s2 * (z23 - z21);

            zcosg = zcosgl;
            zsing = zsingl;
            zcosi = zcosil;
            zsini = zsinil;
            zcosh = zcoshl * cnodm + zsinhl * snodm;
            zsinh = snodm * zcoshl - cnodm * zsinhl;
            cc = C1L;
        } else {
            c.s1 = s1;
            c.s2 = s2;
            c.s3 = s3;
            c.s4 = s4;
            c.s5 = s5;
            c.z1 = z1;
            c.z3 = z3;
            c.z11 = z11;
            c.z13 = z13;
            c.z21 = z21;
            c.z23 = z23;
            c.z31 = z31;
            c.z33 = z33;
            // Lunar terms of dpper.
            d.ee2 = 2.0 * s1 * s6;
            d.e3 = 2.0 * s1 * s7;
            d.xi2 = 2.0 * s2 * z12;
            d.xi3 = 2.0 * s2 * (z13 - z11);
            d.xl2 = -2.0 * s3 * z2;
            d.xl3 = -2.0 * s3 * (z3 - z1);
            d.xl4 = -2.0 * s3 * (-21.0 - 9.0 * emsq) * ZEL;
            d.xgh2 = 2.0 * s4 * z32;
            d.xgh3 = 2.0 * s4 * (z33 - z31);
            d.xgh4 = -18.0 * s4 * ZEL;
            d.xh2 = -2.0 * s2 * z22;
            d.xh3 = -2.0 * s2 * (z23 - z21);
        }
    }
    d.zmol = (4.719_967_2 + 0.229_971_50 * day - gam) % TAU;
    d.zmos = (6.256_583_7 + 0.017_201_977 * day) % TAU;
    c
}

/// Deep-space resonance constants (Vallado's `dsinit`, at t = 0).
#[allow(clippy::too_many_arguments)]
fn dsinit(
    d: &mut Deep,
    c: &Dscom,
    xke: f64,
    argpo: f64,
    gsto: f64,
    mo: f64,
    mdot: f64,
    no: f64,
    nodeo: f64,
    nodedot: f64,
    xpidot: f64,
    ecco: f64,
    eccsq: f64,
    inclm: f64,
) {
    const Q22: f64 = 1.789_167_9e-6;
    const Q31: f64 = 2.146_074_8e-6;
    const Q33: f64 = 2.212_301_5e-7;
    const ROOT22: f64 = 1.789_167_9e-6;
    const ROOT44: f64 = 7.363_695_3e-9;
    const ROOT54: f64 = 2.176_580_3e-9;
    const RPTIM: f64 = 4.375_269_088_011_3e-3;
    const ROOT32: f64 = 3.739_379_2e-7;
    const ROOT52: f64 = 1.142_863_9e-7;
    const ZNL: f64 = 1.583_521_8e-4;
    const ZNS: f64 = 1.194_59e-5;

    let (cosim, sinim, emsq) = (c.cosim, c.sinim, c.emsq);
    let nm = no;
    let em = ecco;

    d.irez = 0;
    if nm < 0.005_235_987_7 && nm > 0.003_490_658_5 {
        d.irez = 1;
    }
    if (8.26e-3..=9.24e-3).contains(&nm) && em >= 0.5 {
        d.irez = 2;
    }

    // Solar terms.
    let ses = c.ss1 * ZNS * c.ss5;
    let sis = c.ss2 * ZNS * (c.sz11 + c.sz13);
    let sls = -ZNS * c.ss3 * (c.sz1 + c.sz3 - 14.0 - 6.0 * emsq);
    let sghs = c.ss4 * ZNS * (c.sz31 + c.sz33 - 6.0);
    let mut shs = -ZNS * c.ss2 * (c.sz21 + c.sz23);
    if !(5.235_987_7e-2..=PI - 5.235_987_7e-2).contains(&inclm) {
        shs = 0.0;
    }
    if sinim != 0.0 {
        shs /= sinim;
    }
    let sgs = sghs - cosim * shs;

    // Lunar terms.
    d.dedt = ses + c.s1 * ZNL * c.s5;
    d.didt = sis + c.s2 * ZNL * (c.z11 + c.z13);
    d.dmdt = sls - ZNL * c.s3 * (c.z1 + c.z3 - 14.0 - 6.0 * emsq);
    let sghl = c.s4 * ZNL * (c.z31 + c.z33 - 6.0);
    let mut shll = -ZNL * c.s2 * (c.z21 + c.z23);
    if !(5.235_987_7e-2..=PI - 5.235_987_7e-2).contains(&inclm) {
        shll = 0.0;
    }
    d.domdt = sgs + sghl;
    d.dnodt = shs;
    if sinim != 0.0 {
        d.domdt -= cosim / sinim * shll;
        d.dnodt += shll / sinim;
    }

    let theta = gsto % TAU;
    if d.irez == 0 {
        return;
    }
    let aonv = (nm / xke).powf(X2O3);

    if d.irez == 2 {
        let cosisq = cosim * cosim;
        let em = ecco;
        let emsq = eccsq;
        let eoc = em * emsq;
        let g201 = -0.306 - (em - 0.64) * 0.440;
        let (g211, g310, g322, g410, g422, g520);
        if em <= 0.65 {
            g211 = 3.616 - 13.2470 * em + 16.2900 * emsq;
            g310 = -19.302 + 117.3900 * em - 228.4190 * emsq + 156.5910 * eoc;
            g322 = -18.9068 + 109.7927 * em - 214.6334 * emsq + 146.5816 * eoc;
            g410 = -41.122 + 242.6940 * em - 471.0940 * emsq + 313.9530 * eoc;
            g422 = -146.407 + 841.8800 * em - 1629.014 * emsq + 1083.4350 * eoc;
            g520 = -532.114 + 3017.977 * em - 5740.032 * emsq + 3708.2760 * eoc;
        } else {
            g211 = -72.099 + 331.819 * em - 508.738 * emsq + 266.724 * eoc;
            g310 = -346.844 + 1582.851 * em - 2415.925 * emsq + 1246.113 * eoc;
            g322 = -342.585 + 1554.908 * em - 2366.899 * emsq + 1215.972 * eoc;
            g410 = -1052.797 + 4758.686 * em - 7193.992 * emsq + 3651.957 * eoc;
            g422 = -3581.690 + 16178.110 * em - 24462.770 * emsq + 12422.520 * eoc;
            g520 = if em > 0.715 {
                -5149.66 + 29936.92 * em - 54087.36 * emsq + 31324.56 * eoc
            } else {
                1464.74 - 4664.75 * em + 3763.64 * emsq
            };
        }
        let (g533, g521, g532);
        if em < 0.7 {
            g533 = -919.22770 + 4988.6100 * em - 9064.7700 * emsq + 5542.21 * eoc;
            g521 = -822.71072 + 4568.6173 * em - 8491.4146 * emsq + 5337.524 * eoc;
            g532 = -853.66600 + 4690.2500 * em - 8624.7700 * emsq + 5341.4 * eoc;
        } else {
            g533 = -37995.780 + 161616.52 * em - 229838.20 * emsq + 109377.94 * eoc;
            g521 = -51752.104 + 218913.95 * em - 309468.16 * emsq + 146349.42 * eoc;
            g532 = -40023.880 + 170470.89 * em - 242699.48 * emsq + 115605.82 * eoc;
        }
        let sini2 = sinim * sinim;
        let f220 = 0.75 * (1.0 + 2.0 * cosim + cosisq);
        let f221 = 1.5 * sini2;
        let f321 = 1.875 * sinim * (1.0 - 2.0 * cosim - 3.0 * cosisq);
        let f322 = -1.875 * sinim * (1.0 + 2.0 * cosim - 3.0 * cosisq);
        let f441 = 35.0 * sini2 * f220;
        let f442 = 39.3750 * sini2 * sini2;
        let f522 = 9.84375
            * sinim
            * (sini2 * (1.0 - 2.0 * cosim - 5.0 * cosisq)
                + 0.33333333 * (-2.0 + 4.0 * cosim + 6.0 * cosisq));
        let f523 = sinim
            * (4.92187512 * sini2 * (-2.0 - 4.0 * cosim + 10.0 * cosisq)
                + 6.56250012 * (1.0 + 2.0 * cosim - 3.0 * cosisq));
        let f542 =
            29.53125 * sinim * (2.0 - 8.0 * cosim + cosisq * (-12.0 + 8.0 * cosim + 10.0 * cosisq));
        let f543 =
            29.53125 * sinim * (-2.0 - 8.0 * cosim + cosisq * (12.0 + 8.0 * cosim - 10.0 * cosisq));
        let xno2 = nm * nm;
        let ainv2 = aonv * aonv;
        let mut temp1 = 3.0 * xno2 * ainv2;
        let mut temp = temp1 * ROOT22;
        d.d2201 = temp * f220 * g201;
        d.d2211 = temp * f221 * g211;
        temp1 *= aonv;
        temp = temp1 * ROOT32;
        d.d3210 = temp * f321 * g310;
        d.d3222 = temp * f322 * g322;
        temp1 *= aonv;
        temp = 2.0 * temp1 * ROOT44;
        d.d4410 = temp * f441 * g410;
        d.d4422 = temp * f442 * g422;
        temp1 *= aonv;
        temp = temp1 * ROOT52;
        d.d5220 = temp * f522 * g520;
        d.d5232 = temp * f523 * g532;
        temp = 2.0 * temp1 * ROOT54;
        d.d5421 = temp * f542 * g521;
        d.d5433 = temp * f543 * g533;
        d.xlamo = (mo + nodeo + nodeo - theta - theta) % TAU;
        d.xfact = mdot + d.dmdt + 2.0 * (nodedot + d.dnodt - RPTIM) - no;
    }

    if d.irez == 1 {
        let g200 = 1.0 + emsq * (-2.5 + 0.8125 * emsq);
        let g310 = 1.0 + 2.0 * emsq;
        let g300 = 1.0 + emsq * (-6.0 + 6.60937 * emsq);
        let f220 = 0.75 * (1.0 + cosim) * (1.0 + cosim);
        let f311 = 0.9375 * sinim * sinim * (1.0 + 3.0 * cosim) - 0.75 * (1.0 + cosim);
        let mut f330 = 1.0 + cosim;
        f330 = 1.875 * f330 * f330 * f330;
        let del1 = 3.0 * nm * nm * aonv * aonv;
        d.del2 = 2.0 * del1 * f220 * g200 * Q22;
        d.del3 = 3.0 * del1 * f330 * g300 * Q33 * aonv;
        d.del1 = del1 * f311 * g310 * Q31 * aonv;
        d.xlamo = (mo + nodeo + argpo - theta) % TAU;
        d.xfact = mdot + xpidot - RPTIM + d.dmdt + d.domdt + d.dnodt - no;
    }
}

/// Deep-space secular effects and resonance (Vallado's `dspace`).
#[allow(clippy::too_many_arguments)]
fn dspace(
    d: &Deep,
    argpo: f64,
    argpdot: f64,
    t: f64,
    gsto: f64,
    no: f64,
    em: &mut f64,
    argpm: &mut f64,
    inclm: &mut f64,
    mm: &mut f64,
    nodem: &mut f64,
    nm: &mut f64,
) {
    const FASX2: f64 = 0.131_309_08;
    const FASX4: f64 = 2.884_319_8;
    const FASX6: f64 = 0.374_480_87;
    const G22: f64 = 5.768_639_6;
    const G32: f64 = 0.952_408_98;
    const G44: f64 = 1.801_499_8;
    const G52: f64 = 1.050_833_0;
    const G54: f64 = 4.410_889_8;
    const RPTIM: f64 = 4.375_269_088_011_3e-3;
    const STEPP: f64 = 720.0;
    const STEPN: f64 = -720.0;
    const STEP2: f64 = 259_200.0;

    let theta = (gsto + t * RPTIM) % TAU;
    *em += d.dedt * t;
    *inclm += d.didt * t;
    *argpm += d.domdt * t;
    *nodem += d.dnodt * t;
    *mm += d.dmdt * t;

    if d.irez == 0 {
        return;
    }
    // Integrate from epoch every time.
    let mut atime = 0.0;
    let mut xni = no;
    let mut xli = d.xlamo;
    let delt = if t > 0.0 { STEPP } else { STEPN };
    let (mut xndt, mut xldot, mut xnddt);
    let ft;
    loop {
        if d.irez != 2 {
            xndt = d.del1 * (xli - FASX2).sin()
                + d.del2 * (2.0 * (xli - FASX4)).sin()
                + d.del3 * (3.0 * (xli - FASX6)).sin();
            xldot = xni + d.xfact;
            xnddt = d.del1 * (xli - FASX2).cos()
                + 2.0 * d.del2 * (2.0 * (xli - FASX4)).cos()
                + 3.0 * d.del3 * (3.0 * (xli - FASX6)).cos();
            xnddt *= xldot;
        } else {
            let xomi = argpo + argpdot * atime;
            let x2omi = xomi + xomi;
            let x2li = xli + xli;
            xndt = d.d2201 * (x2omi + xli - G22).sin()
                + d.d2211 * (xli - G22).sin()
                + d.d3210 * (xomi + xli - G32).sin()
                + d.d3222 * (-xomi + xli - G32).sin()
                + d.d4410 * (x2omi + x2li - G44).sin()
                + d.d4422 * (x2li - G44).sin()
                + d.d5220 * (xomi + xli - G52).sin()
                + d.d5232 * (-xomi + xli - G52).sin()
                + d.d5421 * (xomi + x2li - G54).sin()
                + d.d5433 * (-xomi + x2li - G54).sin();
            xldot = xni + d.xfact;
            xnddt = d.d2201 * (x2omi + xli - G22).cos()
                + d.d2211 * (xli - G22).cos()
                + d.d3210 * (xomi + xli - G32).cos()
                + d.d3222 * (-xomi + xli - G32).cos()
                + d.d5220 * (xomi + xli - G52).cos()
                + d.d5232 * (-xomi + xli - G52).cos()
                + 2.0
                    * (d.d4410 * (x2omi + x2li - G44).cos()
                        + d.d4422 * (x2li - G44).cos()
                        + d.d5421 * (xomi + x2li - G54).cos()
                        + d.d5433 * (-xomi + x2li - G54).cos());
            xnddt *= xldot;
        }
        if (t - atime).abs() >= STEPP {
            xli += xldot * delt + xndt * STEP2;
            xni += xndt * delt + xnddt * STEP2;
            atime += delt;
        } else {
            ft = t - atime;
            break;
        }
    }
    *nm = xni + xndt * ft + xnddt * ft * ft * 0.5;
    let xl = xli + xldot * ft + xndt * ft * ft * 0.5;
    if d.irez != 1 {
        *mm = xl - 2.0 * *nodem + 2.0 * theta;
    } else {
        *mm = xl - *nodem - *argpm + theta;
    }
    let dndt = *nm - no;
    *nm = no + dndt;
}

/// Lunar-solar periodics (Vallado's `dpper`, opsmode 'i').
#[allow(clippy::too_many_arguments)]
fn dpper(
    d: &Deep,
    t: f64,
    init: bool,
    ep: &mut f64,
    inclp: &mut f64,
    nodep: &mut f64,
    argpp: &mut f64,
    mp: &mut f64,
) {
    const ZNS: f64 = 1.194_59e-5;
    const ZES: f64 = 0.01675;
    const ZNL: f64 = 1.583_521_8e-4;
    const ZEL: f64 = 0.05490;

    let mut zm = if init { d.zmos } else { d.zmos + ZNS * t };
    let mut zf = zm + 2.0 * ZES * zm.sin();
    let mut sinzf = zf.sin();
    let mut f2 = 0.5 * sinzf * sinzf - 0.25;
    let mut f3 = -0.5 * sinzf * zf.cos();
    let ses = d.se2 * f2 + d.se3 * f3;
    let sis = d.si2 * f2 + d.si3 * f3;
    let sls = d.sl2 * f2 + d.sl3 * f3 + d.sl4 * sinzf;
    let sghs = d.sgh2 * f2 + d.sgh3 * f3 + d.sgh4 * sinzf;
    let shs = d.sh2 * f2 + d.sh3 * f3;
    zm = if init { d.zmol } else { d.zmol + ZNL * t };
    zf = zm + 2.0 * ZEL * zm.sin();
    sinzf = zf.sin();
    f2 = 0.5 * sinzf * sinzf - 0.25;
    f3 = -0.5 * sinzf * zf.cos();
    let sel = d.ee2 * f2 + d.e3 * f3;
    let sil = d.xi2 * f2 + d.xi3 * f3;
    let sll = d.xl2 * f2 + d.xl3 * f3 + d.xl4 * sinzf;
    let sghl = d.xgh2 * f2 + d.xgh3 * f3 + d.xgh4 * sinzf;
    let shll = d.xh2 * f2 + d.xh3 * f3;
    let mut pe = ses + sel;
    let mut pinc = sis + sil;
    let mut pl = sls + sll;
    let mut pgh = sghs + sghl;
    let mut ph = shs + shll;

    if init {
        return;
    }
    pe -= d.peo;
    pinc -= d.pinco;
    pl -= d.plo;
    pgh -= d.pgho;
    ph -= d.pho;
    *inclp += pinc;
    *ep += pe;
    let sinip = inclp.sin();
    let cosip = inclp.cos();

    if *inclp >= 0.2 {
        ph /= sinip;
        pgh -= cosip * ph;
        *argpp += pgh;
        *nodep += ph;
        *mp += pl;
    } else {
        // Lyddane modification for low inclinations.
        let sinop = nodep.sin();
        let cosop = nodep.cos();
        let mut alfdp = sinip * sinop;
        let mut betdp = sinip * cosop;
        let dalf = ph * cosop + pinc * cosip * sinop;
        let dbet = -ph * sinop + pinc * cosip * cosop;
        alfdp += dalf;
        betdp += dbet;
        *nodep %= TAU;
        let mut xls = *mp + *argpp + cosip * *nodep;
        let dls = pl + pgh - pinc * *nodep * sinip;
        xls += dls;
        let xnoh = *nodep;
        *nodep = alfdp.atan2(betdp);
        if (xnoh - *nodep).abs() > PI {
            if *nodep < xnoh {
                *nodep += TAU;
            } else {
                *nodep -= TAU;
            }
        }
        *mp += pl;
        *argpp = xls - *mp - cosip * *nodep;
    }
}
