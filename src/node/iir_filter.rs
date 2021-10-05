use std::{
    f32::consts::PI,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use crossbeam_channel::{Receiver, Sender};
use num_complex::Complex;

use crate::{
    alloc::AudioBuffer,
    buffer::{ChannelConfig, ChannelConfigOptions},
    context::{AsBaseAudioContext, AudioContextRegistration, AudioParamId},
    param::{AudioParam, AudioParamOptions},
    process::{AudioParamValues, AudioProcessor},
    SampleRate, MAX_CHANNELS,
};

use super::AudioNode;

struct CoeffsReq(Sender<[f32; 6]>);

pub struct IirFilterOptions {
    /// audio node options
    pub channel_config: ChannelConfigOptions,
    /// feedforward coefficients
    pub feedforward: Vec<f64>,
    /// feedback coefficients
    pub feedback: Vec<f64>,
}

/// AudioNode for volume control
pub struct IirFilterNode {
    sample_rate: f32,
    registration: AudioContextRegistration,
    channel_config: ChannelConfig,
    q: AudioParam,
    detune: AudioParam,
    frequency: AudioParam,
    gain: AudioParam,
    type_: Arc<AtomicU32>,
    sender: Sender<CoeffsReq>,
}

impl AudioNode for IirFilterNode {
    fn registration(&self) -> &AudioContextRegistration {
        &self.registration
    }

    fn channel_config_raw(&self) -> &ChannelConfig {
        &self.channel_config
    }

    fn number_of_inputs(&self) -> u32 {
        1
    }
    fn number_of_outputs(&self) -> u32 {
        1
    }
}

impl IirFilterNode {
    pub fn new<C: AsBaseAudioContext>(context: &C, options: Option<IirFilterOptions>) -> Self {
        context.base().register(move |registration| {
            let options = options.unwrap_or_default();

            let sample_rate = context.base().sample_rate().0 as f32;

            let default_freq = 350.;
            let default_gain = 0.;
            let default_det = 0.;
            let default_q = 1.;

            let q_value = options.detune.unwrap_or(default_det);
            let d_value = options.detune.unwrap_or(default_det);
            let f_value = options.frequency.unwrap_or(default_freq);
            let g_value = options.gain.unwrap_or(default_gain);
            let t_value = options.type_.unwrap_or(IirFilterType::Lowpass);

            let q_param_opts = AudioParamOptions {
                min_value: f32::MIN,
                max_value: f32::MAX,
                default_value: default_q,
                automation_rate: crate::param::AutomationRate::A,
            };
            let (q_param, q_proc) = context
                .base()
                .create_audio_param(q_param_opts, registration.id());

            q_param.set_value(q_value);

            let d_param_opts = AudioParamOptions {
                min_value: -153600.,
                max_value: 153600.,
                default_value: default_det,
                automation_rate: crate::param::AutomationRate::A,
            };
            let (d_param, d_proc) = context
                .base()
                .create_audio_param(d_param_opts, registration.id());

            d_param.set_value(d_value);

            let niquyst = context.base().sample_rate().0 / 2;
            let f_param_opts = AudioParamOptions {
                min_value: 0.,
                max_value: niquyst as f32,
                default_value: default_freq,
                automation_rate: crate::param::AutomationRate::A,
            };
            let (f_param, f_proc) = context
                .base()
                .create_audio_param(f_param_opts, registration.id());

            f_param.set_value(f_value);

            let g_param_opts = AudioParamOptions {
                min_value: f32::MIN,
                max_value: f32::MAX,
                default_value: default_gain,
                automation_rate: crate::param::AutomationRate::A,
            };
            let (g_param, g_proc) = context
                .base()
                .create_audio_param(g_param_opts, registration.id());

            g_param.set_value(g_value);

            let type_ = Arc::new(AtomicU32::new(t_value as u32));

            let inits = Params {
                q: q_value,
                detune: d_value,
                frequency: f_value,
                gain: g_value,
                type_: t_value,
            };

            let (sender, receiver) = crossbeam_channel::bounded(0);

            let config = RendererConfig {
                sample_rate,
                gain: g_proc,
                detune: d_proc,
                frequency: f_proc,
                q: q_proc,
                type_: type_.clone(),
                params: inits,
                receiver,
            };

            let render = IirFilterRenderer::new(config);
            let node = IirFilterNode {
                sample_rate,
                registration,
                channel_config: options.channel_config.into(),
                type_,
                q: q_param,
                detune: d_param,
                frequency: f_param,
                gain: g_param,
                sender,
            };

            (node, Box::new(render))
        })
    }

    /// Returns the gain audio paramter
    pub fn gain(&self) -> &AudioParam {
        &self.gain
    }

    /// Returns the frequency audio paramter
    pub fn frequency(&self) -> &AudioParam {
        &self.frequency
    }

    /// Returns the detune audio paramter
    pub fn detune(&self) -> &AudioParam {
        &self.detune
    }

    /// Returns the Q audio paramter
    pub fn q(&self) -> &AudioParam {
        &self.q
    }

    /// Returns the biquad filter type
    pub fn type_(&self) -> IirFilterType {
        self.type_.load(Ordering::SeqCst).into()
    }

    /// biquad filter type setter
    ///
    /// # Arguments
    ///
    /// * `type_` - the biquad filter type (lowpass, highpass,...)
    pub fn set_type(&mut self, type_: IirFilterType) {
        self.type_.store(type_ as u32, Ordering::SeqCst);
    }

    /// Returns the frequency response for the specified frequencies
    ///
    /// # Arguments
    ///
    /// * `frequency_hz` - frequencies for which frequency response of the filter should be calculated
    /// * `mag_response` - magnitude of the frequency response of the filter
    /// * `phase_response` - phase of the frequency response of the filter
    pub fn get_frequency_response(
        &self,
        frequency_hz: &[f32],
        mag_response: &mut [f32],
        phase_response: &mut [f32],
    ) {
        let (sender, receiver) = crossbeam_channel::bounded(0);
        self.sender.send(CoeffsReq(sender)).unwrap();

        loop {
            match receiver.try_recv() {
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    panic!("Receiver Error: disconnected type");
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    println!("Receiver Error: empty type");
                    continue;
                }
                Ok([b0, b1, b2, a0, a1, a2]) => {
                    for (i, &f) in frequency_hz.iter().enumerate() {
                        let num = b0
                            + Complex::from_polar(b1, -1.0 * 2.0 * PI * f / self.sample_rate)
                            + Complex::from_polar(b2, -2.0 * 2.0 * PI * f / self.sample_rate);
                        let denom = a0
                            + Complex::from_polar(a1, -1.0 * 2.0 * PI * f / self.sample_rate)
                            + Complex::from_polar(a2, -2.0 * 2.0 * PI * f / self.sample_rate);
                        let h_f = num / denom;

                        mag_response[i] = h_f.norm();
                        phase_response[i] = h_f.arg()
                    }
                    break;
                }
            }
        }
    }
}

struct Params {
    q: f32,
    detune: f32,
    frequency: f32,
    gain: f32,
    type_: IirFilterType,
}

struct RendererConfig {
    sample_rate: f32,
    q: AudioParamId,
    detune: AudioParamId,
    frequency: AudioParamId,
    gain: AudioParamId,
    type_: Arc<AtomicU32>,
    params: Params,
    receiver: Receiver<CoeffsReq>,
}

/// Biquad filter coefficients
#[derive(Clone, Copy, Debug)]
struct Coefficients {
    // Denominator coefficients
    a0: f32,
    a1: f32,
    a2: f32,

    // Nominator coefficients
    b0: f32,
    b1: f32,
    b2: f32,
}

struct IirFilterRenderer {
    sample_rate: f32,
    q: AudioParamId,
    detune: AudioParamId,
    frequency: AudioParamId,
    gain: AudioParamId,
    type_: Arc<AtomicU32>,
    ss1: [f32; MAX_CHANNELS],
    ss2: [f32; MAX_CHANNELS],
    coeffs: Coefficients,
    receiver: Receiver<CoeffsReq>,
}

impl AudioProcessor for IirFilterRenderer {
    fn process(
        &mut self,
        inputs: &[crate::alloc::AudioBuffer],
        outputs: &mut [crate::alloc::AudioBuffer],
        params: AudioParamValues,
        _timestamp: f64,
        _sample_rate: SampleRate,
    ) {
        // single input/output node
        let input = &inputs[0];
        let output = &mut outputs[0];

        let g_values = params.get(&self.gain);
        let det_values = params.get(&self.detune);
        let freq_values = params.get(&self.frequency);
        let q_values = params.get(&self.q);
        let type_ = self.type_.load(Ordering::SeqCst).into();

        let params = Params {
            q: q_values[0],
            detune: det_values[0],
            frequency: freq_values[0],
            gain: g_values[0],
            type_,
        };

        self.filter(input, output, params);
    }

    fn tail_time(&self) -> bool {
        false
    }
}

impl IirFilterRenderer {
    fn new(config: RendererConfig) -> Self {
        let RendererConfig {
            sample_rate,
            q,
            detune,
            frequency,
            gain,
            type_,
            params,
            receiver,
        } = config;

        let coeffs = Self::init_coeffs(sample_rate, params);

        let s1 = [0.; MAX_CHANNELS];
        let s2 = [0.; MAX_CHANNELS];

        Self {
            sample_rate,
            gain,
            detune,
            frequency,
            q,
            type_,
            ss1: s1,
            ss2: s2,
            coeffs,
            receiver,
        }
    }

    /// Generate an output by filtering the input following the params values
    ///
    /// # Arguments
    ///
    /// * `input` - Audiobuffer input
    /// * `output` - Audiobuffer output
    /// * `params` - IirFilter params which resolves into biquad coeffs
    fn filter(&mut self, input: &AudioBuffer, output: &mut AudioBuffer, params: Params) {
        // todo : A-rate
        self.update_coeffs(params);

        let Coefficients {
            b0,
            b1,
            b2,
            a0,
            a1,
            a2,
        } = self.coeffs;

        let coeffs_resp = [b0, b1, b2, a0, a1, a2];

        if let Ok(msg) = self.receiver.try_recv() {
            let sender = msg.0;

            sender.send(coeffs_resp).unwrap();
        }

        for (idx, (i_data, o_data)) in input
            .channels()
            .iter()
            .zip(output.channels_mut())
            .enumerate()
        {
            for (&i, o) in i_data.iter().zip(o_data.iter_mut()) {
                *o = self.tick(i, idx);
            }
        }
    }

    /// Generate an output sample by filtering an input sample
    ///
    /// # Arguments
    ///
    /// * `input` - Audiobuffer input
    /// * `idx` - channel index mapping to the filter state index
    fn tick(&mut self, input: f32, idx: usize) -> f32 {
        let out = self.ss1[idx] + (self.coeffs.b0 / self.coeffs.a0) * input;
        self.ss1[idx] = self.ss2[idx] + (self.coeffs.b1 / self.coeffs.a0) * input
            - (self.coeffs.a1 / self.coeffs.a0) * out;
        self.ss2[idx] =
            (self.coeffs.b2 / self.coeffs.a0) * input - (self.coeffs.a2 / self.coeffs.a0) * out;

        out
    }

    /// initializes biquad filter coefficients
    ///
    /// # Arguments
    ///
    /// * `sample_rate` - Audio context sample rate
    /// * `params` - params resolving into biquad coeffs
    fn init_coeffs(sample_rate: f32, params: Params) -> Coefficients {
        let Params {
            q,
            detune,
            frequency,
            gain,
            type_,
        } = params;

        let computed_freq = frequency * 10f32.powf(detune / 1200.);

        let b0 = Self::b0(type_, sample_rate, computed_freq, q, gain);
        let b1 = Self::b1(type_, sample_rate, computed_freq, gain);
        let b2 = Self::b2(type_, sample_rate, computed_freq, q, gain);

        let a0 = Self::a0(type_, sample_rate, computed_freq, q, gain);
        let a1 = Self::a1(type_, sample_rate, computed_freq, gain);
        let a2 = Self::a2(type_, sample_rate, computed_freq, q, gain);

        Coefficients {
            b0,
            b1,
            b2,
            a0,
            a1,
            a2,
        }
    }

    /// updates biquad filter coefficients when params are modified
    ///
    /// # Arguments
    ///
    /// * `params` - params resolving into biquad coeffs
    fn update_coeffs(&mut self, params: Params) {
        let Params {
            q,
            detune,
            frequency,
            gain,
            type_,
        } = params;

        let computed_freq = frequency * 10f32.powf(detune / 1200.);

        self.coeffs.b0 = Self::b0(type_, self.sample_rate, computed_freq, q, gain);
        self.coeffs.b1 = Self::b1(type_, self.sample_rate, computed_freq, gain);
        self.coeffs.b2 = Self::b2(type_, self.sample_rate, computed_freq, q, gain);
        self.coeffs.a0 = Self::a0(type_, self.sample_rate, computed_freq, q, gain);
        self.coeffs.a1 = Self::a1(type_, self.sample_rate, computed_freq, gain);
        self.coeffs.a2 = Self::a2(type_, self.sample_rate, computed_freq, q, gain);
    }

    /// calculates b_0 coefficient
    ///
    /// # Arguments
    ///
    /// * `type_` - IirFilter type
    /// * `sample_rate` - audio context sample rate
    /// * `computed_freq` - computedOscFreq
    /// * `q` - Q factor
    /// * `gain` - filter gain
    fn b0(type_: IirFilterType, sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        match type_ {
            IirFilterType::Lowpass => Self::b0_lowpass(sample_rate, computed_freq),
            IirFilterType::Highpass => Self::b0_highpass(sample_rate, computed_freq),
            IirFilterType::Bandpass => Self::b0_bandpass(sample_rate, computed_freq, q),
            IirFilterType::Notch => Self::b0_notch(),
            IirFilterType::Allpass => Self::b0_allpass(sample_rate, computed_freq, q),
            IirFilterType::Peaking => Self::b0_peaking(sample_rate, computed_freq, q, gain),
            IirFilterType::Lowshelf => Self::b0_lowshelf(sample_rate, computed_freq, gain),
            IirFilterType::Highshelf => Self::b0_highshelf(sample_rate, computed_freq, gain),
        }
    }

    fn b0_lowpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        (1.0 - w0.cos()) / 2.0
    }

    fn b0_highpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        (1.0 + w0.cos()) / 2.0
    }

    fn b0_bandpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        Self::alpha_q(sample_rate, computed_freq, q)
    }

    fn b0_notch() -> f32 {
        1.0
    }

    fn b0_allpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        1.0 - alpha_q
    }

    fn b0_peaking(sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        let a = Self::a(gain);
        1.0 + alpha_q * a
    }

    fn b0_lowshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        a * ((a + 1.0) - (a - 1.0) * w0.cos() + 2.0 * alpha_s * a.sqrt())
    }

    fn b0_highshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        a * ((a + 1.0) + (a - 1.0) * w0.cos() + 2.0 * alpha_s * a.sqrt())
    }

    /// calculates b_1 coefficient
    ///
    /// # Arguments
    ///
    /// * `type_` - IirFilter type
    /// * `sample_rate` - audio context sample rate
    /// * `computed_freq` - computedOscFreq
    /// * `gain` - filter gain
    fn b1(type_: IirFilterType, sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        match type_ {
            IirFilterType::Lowpass => Self::b1_lowpass(sample_rate, computed_freq),
            IirFilterType::Highpass => Self::b1_highpass(sample_rate, computed_freq),
            IirFilterType::Bandpass => Self::b1_bandpass(),
            IirFilterType::Notch => Self::b1_notch(sample_rate, computed_freq),
            IirFilterType::Allpass => Self::b1_allpass(sample_rate, computed_freq),
            IirFilterType::Peaking => Self::b1_peaking(sample_rate, computed_freq),
            IirFilterType::Lowshelf => Self::b1_lowshelf(sample_rate, computed_freq, gain),
            IirFilterType::Highshelf => Self::b1_highshelf(sample_rate, computed_freq, gain),
        }
    }

    fn b1_lowpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        1.0 - w0.cos()
    }

    fn b1_highpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -(1.0 + w0.cos())
    }

    fn b1_bandpass() -> f32 {
        0.0
    }

    fn b1_notch(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn b1_allpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn b1_peaking(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn b1_lowshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        2.0 * a * ((a - 1.0) - (a + 1.0) * w0.cos())
    }

    fn b1_highshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * a * ((a - 1.0) + (a + 1.0) * w0.cos())
    }

    /// calculates b_2 coefficient
    ///
    /// # Arguments
    ///
    /// * `type_` - IirFilter type
    /// * `sample_rate` - audio context sample rate
    /// * `computed_freq` - computedOscFreq
    /// * `q` - Q factor
    /// * `gain` - filter gain
    fn b2(type_: IirFilterType, sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        match type_ {
            IirFilterType::Lowpass => Self::b2_lowpass(sample_rate, computed_freq),
            IirFilterType::Highpass => Self::b2_highpass(sample_rate, computed_freq),
            IirFilterType::Bandpass => Self::b2_bandpass(sample_rate, computed_freq, q),
            IirFilterType::Notch => Self::b2_notch(),
            IirFilterType::Allpass => Self::b2_allpass(sample_rate, computed_freq, q),
            IirFilterType::Peaking => Self::b2_peaking(sample_rate, computed_freq, q, gain),
            IirFilterType::Lowshelf => Self::b2_lowshelf(sample_rate, computed_freq, gain),
            IirFilterType::Highshelf => Self::b2_highshelf(sample_rate, computed_freq, gain),
        }
    }

    fn b2_lowpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        (1.0 - w0.cos()) / 2.0
    }

    fn b2_highpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        (1.0 + w0.cos()) / 2.0
    }

    fn b2_bandpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        -Self::alpha_q(sample_rate, computed_freq, q)
    }

    fn b2_notch() -> f32 {
        1.0
    }

    fn b2_allpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        1.0 + alpha_q
    }

    fn b2_peaking(sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        let a = Self::a(gain);
        1.0 - alpha_q * a
    }

    fn b2_lowshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        a * ((a + 1.0) - (a - 1.0) * w0.cos() - 2.0 * alpha_s * a.sqrt())
    }

    fn b2_highshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        a * ((a + 1.0) + (a - 1.0) * w0.cos() - 2.0 * alpha_s * a.sqrt())
    }

    /// calculates a_0 coefficient
    ///
    /// # Arguments
    ///
    /// * `type_` - IirFilter type
    /// * `sample_rate` - audio context sample rate
    /// * `computed_freq` - computedOscFreq
    /// * `q` - Q factor
    /// * `gain` - filter gain
    fn a0(type_: IirFilterType, sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        match type_ {
            IirFilterType::Lowpass => Self::a0_lowpass(sample_rate, computed_freq, q),
            IirFilterType::Highpass => Self::a0_highpass(sample_rate, computed_freq, q),
            IirFilterType::Bandpass => Self::a0_bandpass(sample_rate, computed_freq, q),
            IirFilterType::Notch => Self::a0_notch(sample_rate, computed_freq, q),
            IirFilterType::Allpass => Self::a0_allpass(sample_rate, computed_freq, q),
            IirFilterType::Peaking => Self::a0_peaking(sample_rate, computed_freq, q, gain),
            IirFilterType::Lowshelf => Self::a0_lowshelf(sample_rate, computed_freq, gain),
            IirFilterType::Highshelf => Self::a0_highshelf(sample_rate, computed_freq, gain),
        }
    }

    fn a0_lowpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q_db = Self::alpha_q_db(sample_rate, computed_freq, q);
        1.0 + alpha_q_db
    }

    fn a0_highpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q_db = Self::alpha_q_db(sample_rate, computed_freq, q);
        1.0 + alpha_q_db
    }

    fn a0_bandpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        1.0 + alpha_q
    }

    fn a0_notch(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        1.0 + alpha_q
    }

    fn a0_allpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        1.0 + alpha_q
    }

    fn a0_peaking(sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        let a = Self::a(gain);
        1.0 + (alpha_q / a)
    }

    fn a0_lowshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        (a + 1.0) + (a - 1.0) * w0.cos() + 2.0 * alpha_s * a.sqrt()
    }

    fn a0_highshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        (a + 1.0) - (a - 1.0) * w0.cos() + 2.0 * alpha_s * a.sqrt()
    }

    /// calculates a_1 coefficient
    ///
    /// # Arguments
    ///
    /// * `type_` - IirFilter type
    /// * `sample_rate` - audio context sample rate
    /// * `computed_freq` - computedOscFreq
    /// * `gain` - filter gain
    fn a1(type_: IirFilterType, sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        match type_ {
            IirFilterType::Lowpass => Self::a1_lowpass(sample_rate, computed_freq),
            IirFilterType::Highpass => Self::a1_highpass(sample_rate, computed_freq),
            IirFilterType::Bandpass => Self::a1_bandpass(sample_rate, computed_freq),
            IirFilterType::Notch => Self::a1_notch(sample_rate, computed_freq),
            IirFilterType::Allpass => Self::a1_allpass(sample_rate, computed_freq),
            IirFilterType::Peaking => Self::a1_peaking(sample_rate, computed_freq),
            IirFilterType::Lowshelf => Self::a1_lowshelf(sample_rate, computed_freq, gain),
            IirFilterType::Highshelf => Self::a1_highshelf(sample_rate, computed_freq, gain),
        }
    }

    fn a1_lowpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn a1_highpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn a1_bandpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn a1_notch(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn a1_allpass(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn a1_peaking(sample_rate: f32, computed_freq: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        -2.0 * w0.cos()
    }

    fn a1_lowshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);

        -2.0 * ((a - 1.0) + (a + 1.0) * w0.cos())
    }

    fn a1_highshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);

        2.0 * ((a - 1.0) - (a + 1.0) * w0.cos())
    }

    /// calculates a_2 coefficient
    ///
    /// # Arguments
    ///
    /// * `type_` - IirFilter type
    /// * `sample_rate` - audio context sample rate
    /// * `computed_freq` - computedOscFreq
    /// * `q` - Q factor
    /// * `gain` - filter gain
    fn a2(type_: IirFilterType, sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        match type_ {
            IirFilterType::Lowpass => Self::a2_lowpass(sample_rate, computed_freq, q),
            IirFilterType::Highpass => Self::a2_highpass(sample_rate, computed_freq, q),
            IirFilterType::Bandpass => Self::a2_bandpass(sample_rate, computed_freq, q),
            IirFilterType::Notch => Self::a2_notch(sample_rate, computed_freq, q),
            IirFilterType::Allpass => Self::a2_allpass(sample_rate, computed_freq, q),
            IirFilterType::Peaking => Self::a2_peaking(sample_rate, computed_freq, q, gain),
            IirFilterType::Lowshelf => Self::a2_lowshelf(sample_rate, computed_freq, gain),
            IirFilterType::Highshelf => Self::a2_highshelf(sample_rate, computed_freq, gain),
        }
    }

    fn a2_lowpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q_db = Self::alpha_q_db(sample_rate, computed_freq, q);
        1.0 - alpha_q_db
    }

    fn a2_highpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q_db = Self::alpha_q_db(sample_rate, computed_freq, q);
        1.0 - alpha_q_db
    }

    fn a2_bandpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q_db = Self::alpha_q_db(sample_rate, computed_freq, q);
        1.0 - alpha_q_db
    }

    fn a2_notch(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q_db = Self::alpha_q_db(sample_rate, computed_freq, q);
        1.0 - alpha_q_db
    }

    fn a2_allpass(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        let alpha_q_db = Self::alpha_q_db(sample_rate, computed_freq, q);
        1.0 - alpha_q_db
    }

    fn a2_peaking(sample_rate: f32, computed_freq: f32, q: f32, gain: f32) -> f32 {
        let alpha_q = Self::alpha_q(sample_rate, computed_freq, q);
        let a = Self::a(gain);
        1.0 - (alpha_q / a)
    }

    fn a2_lowshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        (a + 1.0) + (a - 1.0) * w0.cos() - 2.0 * alpha_s * a.sqrt()
    }

    fn a2_highshelf(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let a = Self::a(gain);
        let w0 = Self::w0(sample_rate, computed_freq);
        let alpha_s = Self::alpha_s(sample_rate, computed_freq, gain);

        (a + 1.0) - (a - 1.0) * w0.cos() - 2.0 * alpha_s * a.sqrt()
    }

    /// Returns A parameter used to calculate biquad coeffs
    fn a(gain: f32) -> f32 {
        10f32.powf(gain / 40.)
    }

    /// Returns w0 (omega 0) parameter used to calculate biquad coeffs
    fn w0(sample_rate: f32, computed_freq: f32) -> f32 {
        2.0 * PI * computed_freq / sample_rate
    }

    /// Returns alpha_q parameter used to calculate biquad coeffs
    fn alpha_q(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        Self::w0(sample_rate, computed_freq).sin() / (2. * q)
    }

    /// Returns alpha_q_db parameter used to calculate biquad coeffs
    fn alpha_q_db(sample_rate: f32, computed_freq: f32, q: f32) -> f32 {
        Self::w0(sample_rate, computed_freq).sin() / (2. * 10f32.powf(q / 20.))
    }

    /// Returns S parameter used to calculate biquad coeffs
    fn s() -> f32 {
        1.0
    }

    /// Returns alpha_S parameter used to calculate biquad coeffs
    fn alpha_s(sample_rate: f32, computed_freq: f32, gain: f32) -> f32 {
        let w0 = Self::w0(sample_rate, computed_freq);
        let a = Self::a(gain);
        let s = Self::s();

        (w0.sin() / 2.0) * ((a + (1. / a)) * ((1. / s) - 1.0) + 2.0)
    }
}
