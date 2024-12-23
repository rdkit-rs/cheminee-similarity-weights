use std::fs::read_to_string;
use ndarray::Array2;
use std::str::FromStr;
use tensorflow::{DataType, Graph, ops, SavedModelBundle, Scope, Session, SessionOptions, SessionRunArgs, Tensor};

pub struct EncoderModel {
    encoder: SavedModelBundle,
    graph: Graph,
}

lazy_static::lazy_static! {
    static ref ASSETS_PATH: String = get_assets_path().unwrap();
    static ref CENTROIDS: Tensor<f32> = load_cluster_centroids().unwrap();
    pub static ref NUM_CLUSTERS: f32 = CENTROIDS.dims()[0] as f32;
}

impl EncoderModel {
    pub fn transform(&self, input_data: &[Vec<i64>]) -> eyre::Result<Vec<Vec<i32>>> {
        let lf_array = self.encode(input_data)?;
        let cols = lf_array.dims()[1];

        let ranked_cluster_labels = lf_array
            .chunks(cols as usize)
            .map(|row_vec| {
                let row_tensor = Tensor::new(&[1, cols]).with_values(row_vec);

                match row_tensor {
                    Ok(row_tensor) => {
                        let cluster_labels = assign_cluster_labels(&row_tensor);

                        cluster_labels.unwrap_or_else(|e| {
                            log::info!("Failed to retrieve cluster labels: {e}");
                            vec![]
                        })
                    },
                    Err(e) => {
                        log::info!("Failed to retrieve tensor row: {e}");
                        vec![]
                    },
                }
            }).collect::<Vec<Vec<i32>>>();

        Ok(ranked_cluster_labels)
    }

    fn encode(&self, input_data: &[Vec<i64>]) -> eyre::Result<Tensor<f32>> {
        let rows = input_data.len() as u64;
        let cols = input_data[0].len() as u64;

        let flattened_input = input_data.concat();
        let input_tensor = Tensor::new(&[rows, cols]).with_values(&flattened_input)?;

        let input_operation = self
            .graph
            .operation_by_name("serving_default_dense_input")?
            .ok_or(eyre::eyre!("No operation found"))?;

        let output_operation = self
            .graph
            .operation_by_name("StatefulPartitionedCall")?
            .ok_or(eyre::eyre!("No operation found"))?;

        let mut run_args = SessionRunArgs::new();
        run_args.add_feed(&input_operation, 0, &input_tensor);

        let output_token = run_args.request_fetch(&output_operation, 0);
        self.encoder.session.run(&mut run_args)?;

        let output_tensor = run_args.fetch(output_token)?;
        Ok(output_tensor)
    }
}

pub fn build_encoder_model() -> eyre::Result<EncoderModel> {
    let (encoder, graph) = load_encoder_model()?;

    Ok(
        EncoderModel {
            encoder,
            graph,
        }
    )
}

fn assign_cluster_labels(lf_array: &Tensor<f32>) -> eyre::Result<Vec<i32>> {
    let mut scope = Scope::new_root_scope();
    let mut run_args = SessionRunArgs::new();

    let centroids_input = ops::Placeholder::new()
        .dtype(DataType::Float)
        .shape(CENTROIDS.dims())
        .build(&mut scope)?;

    let lf_input = ops::Placeholder::new()
        .dtype(DataType::Float)
        .shape(lf_array.dims())
        .build(&mut scope)?;

    run_args.add_feed(&centroids_input, 0, &CENTROIDS);
    run_args.add_feed(&lf_input, 0, lf_array);

    let begin_tensor = ops::Const::new()
        .dtype(DataType::Int32)
        .value(Tensor::new(&[2]).with_values(&[0, 0])?)
        .build(&mut scope)?;

    let size_tensor = ops::Const::new()
        .dtype(DataType::Int32)
        .value(Tensor::new(&[2]).with_values(&[1, 128])?)
        .build(&mut scope)?;

    let lf_slice = ops::Slice::new()
        .build(lf_input, begin_tensor, size_tensor, &mut scope)?;

    let diff = ops::Sub::new()
        .build(centroids_input, lf_slice, &mut scope)?;

    let squared_diff = ops::Square::new()
        .build(diff, &mut scope)?;

    let axis_tensor = ops::Const::new()
        .dtype(DataType::Int32)
        .value(Tensor::new(&[1]).with_values(&[1])?)
        .build(&mut scope)?;

    let mean_squared_diff = ops::Mean::new()
        .build(squared_diff, axis_tensor, &mut scope)?;

    let distance = ops::Sqrt::new()
        .build(mean_squared_diff, &mut scope)?;

    let negated_distance = ops::Neg::new()
        .build(distance, &mut scope)?;

    let k_tensor = ops::Const::new()
        .dtype(DataType::Int64)
        .value(CENTROIDS.dims()[0] as i64)
        .build(&mut scope)?;

    let top_k = ops::TopKV2::new()
        .build(negated_distance, k_tensor, &mut scope)?;

    let graph = scope.graph();
    let session = Session::new(&SessionOptions::new(), &graph)?;

    let top_k_token = run_args.request_fetch(&top_k, 1);
    session.run(&mut run_args)?;

    let ranked_cluster_labels = run_args.fetch(top_k_token)?;
    let ranked_cluster_labels = ranked_cluster_labels.iter().as_slice().to_vec();

    Ok(ranked_cluster_labels)
}

fn load_cluster_centroids() -> eyre::Result<Tensor<f32>> {
    let centroids_path = format!("{}/lf_kmeans_10k_centroids_20241111.csv", ASSETS_PATH.as_str());

    let centroid_vec = read_to_string(centroids_path)?
        .lines()
        .map(|line| {
            line.split(',')
                .map(|value| f32::from_str(value.trim()).unwrap())
                .collect()
        })
        .collect::<Vec<Vec<f32>>>();

    let array: Array2<f32> = Array2::from_shape_vec((centroid_vec.len(), centroid_vec[0].len()), centroid_vec.concat())?;
    let array_slice = array.as_slice().ok_or(eyre::eyre!("Failed to convert array to slice"))?;

    let tensor = Tensor::new(&[array.shape()[0] as u64, array.shape()[1] as u64])
        .with_values(array_slice)?;

    Ok(tensor)
}

fn load_encoder_model() -> eyre::Result<(SavedModelBundle, Graph)> {
    let session_options = SessionOptions::new();
    let mut graph = Graph::new();
    let model_dir = format!("{}/vae_encoder", ASSETS_PATH.as_str());
    let saved_model = SavedModelBundle::load(&session_options, vec!["serve"], &mut graph, model_dir)?;

    Ok((saved_model, graph))
}

pub fn get_assets_path() -> eyre::Result<String> {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let target_dir = format!("{}/target", crate_dir);
    let build_type = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };

    let build_dir = format!("{}/{}/build", target_dir, build_type);
    let entries = std::fs::read_dir(build_dir)?;

    let mut assets_path = "".to_string();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();

                if path.is_dir() {
                    if let Some(dir_name) = path.file_name() {
                        if dir_name.to_string_lossy().starts_with("cheminee-similarity-model-") {
                            let out_dir = path.join("out");
                            if out_dir.is_dir() {
                                let out_path = out_dir.to_string_lossy().to_string();
                                assets_path = format!("{}/assets", out_path);
                            }
                        }
                    }
                }
            },
            Err(e) => return Err(eyre::eyre!("Caught an exception while searching for the assets path: {}", e))
        }
    }

    if assets_path.is_empty() {
        return Err(eyre::eyre!("Failed to find assets path"))
    }

    Ok(assets_path)
}
