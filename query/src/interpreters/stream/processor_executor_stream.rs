// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use common_datablocks::DataBlock;
use common_exception::Result;
use futures::Stream;

use crate::pipelines::new::executor::PipelinePullingExecutor;

pub struct ProcessorExecutorStream {
    executor: PipelinePullingExecutor,
}

impl ProcessorExecutorStream {
    pub fn create(mut executor: PipelinePullingExecutor) -> Result<Self> {
        executor.start();
        Ok(Self { executor })
    }
}

impl Stream for ProcessorExecutorStream {
    type Item = Result<DataBlock>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let self_ = Pin::get_mut(self);
        match self_.executor.pull_data() {
            Err(cause) => Poll::Ready(Some(Err(cause))),
            Ok(Some(data)) => Poll::Ready(Some(Ok(data))),
            Ok(None) => Poll::Ready(None),
        }
    }
}
