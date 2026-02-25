#[derive(clap::Parser)]
pub struct Args {
    /// Path to the audio file to be played
    pub file_path: String,

    /// Name of the audio source node to use as the reference "microphone". If not specified, the default
    /// audio source node will be used.
    #[arg(short, long)]
    pub audio_source_name: Option<String>,
}
